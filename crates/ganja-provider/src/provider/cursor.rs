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
//! hand-written in [`connect`] over the same `reqwest`/rustls stack every
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
//! moment the transport hands bytes over ([`connect::Splitter`]), each frame
//! mapped onto events ([`decode::Mapping`]) and handed to the session while
//! the server is still talking. The request that opens the exchange rides
//! the shared retry driver every other wire does — before the first byte
//! only — and a cancel mid-stream ends the stream without a verdict, which
//! is the engine's cue to call the turn cancelled rather than failed. What
//! is still deliberately not here is the conversation-state machinery that
//! carries history and tool calls on cursor's content-addressed channel;
//! [`request`]'s module docs say why.
//!
//! The provider rides the uncataloged tier, so a session must be told which
//! model to ask for; [`CursorWire::usable_models`] is the listing that says
//! what the seat may name. Construction reads nothing — grok's posture — and
//! the stored login is read per request, so a login that happens after a
//! session starts is picked up by its next request.

use std::{collections::VecDeque, fmt, sync::Arc};

use async_trait::async_trait;
use futures::{Stream, StreamExt as _, stream, stream::BoxStream};
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, RefreshOauth},
    provider::{
        ChatRequest, CredentialSource, Presented, Provider, ProviderError, ProviderEvent,
        check_base_url, client, is_terminal, retry, shown_base_url,
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
/// The allow is for the generated decoders of fieldless messages, whose
/// unknown-field handling is a one-arm match: the codegen's own allow list
/// covers its view module but not these, and the alternative is hand-editing
/// a file whose whole contract is that nobody does.
#[allow(clippy::match_single_binding)]
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
        let stored = auth::storage_key(ID);
        let listed =
            auth::list_providers().map_err(|error| ProviderError::Auth(error.to_string()))?;
        if !listed.iter().any(|entry| entry.provider_id == stored) {
            return Err(ProviderError::Auth(format!(
                "no {ID} credential is stored; run `ganja auth login {ID}`"
            )));
        }

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
        // on.
        let built = self.build(MODELS_PATH, false, Vec::new(), &presented)?;
        // A listing has no cancel channel of its own, so the retry driver
        // rides under a token nothing fires.
        let never = CancellationToken::new();
        let response = retry::send(&self.client, built, &presented, &never).await?;
        let body = response.bytes().await.map_err(transport)?;

        decode::model_list(&body)
    }

    /// One turn: the run request out, events in as the server produces them.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the turn cannot start — no credential,
    /// an endpoint that refused or never answered, every retry spent.
    /// Everything after the first byte arrives inside the stream instead: an
    /// in-body EndStream verdict, a dead connection and a frame this build
    /// cannot read all end it with [`ProviderEvent::Failed`], because by
    /// then text may already be on somebody's screen.
    pub async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let message = request::run_message(&request)?;
        let presented = self.credential.presented().await?;
        let built = self.build(RUN_PATH, true, connect::envelope(&message), &presented)?;

        let response = match retry::send(&self.client, built, &presented, &cancel).await {
            Ok(response) => response,
            // A turn the user already left is not a failed one: the engine
            // reads a stream that ends after a cancel as `Cancelled`.
            Err(_) if cancel.is_cancelled() => return Ok(stream::empty().boxed()),
            Err(error) => return Err(error),
        };

        Ok(events(response.bytes_stream().boxed(), cancel))
    }

    /// Builds one RPC's request: the recorded header set, verbatim, over
    /// `body`.
    ///
    /// The split between the two content types — and
    /// `connect-protocol-version` on the streaming RPC only — is exactly
    /// what the live probe measured the server enforcing. The
    /// `x-request-id` stamp is minted here, once per turn: the retry
    /// driver's replay is a clone of this request, so a retried request is
    /// the same request under the same id rather than a new one wearing a
    /// fresh stamp.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no request id can be
    /// minted or the request cannot be assembled; nothing was sent.
    fn build(
        &self,
        path: &str,
        streaming: bool,
        body: Vec<u8>,
        presented: &Presented,
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
            .header("x-request-id", request::fresh_id()?)
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

/// Drives the body's chunks through the Connect splitter and the mapping,
/// handing out each frame's events the moment the frame completes.
///
/// The cancellation posture is the SSE fold's, verbatim: the token is
/// checked before handing out a buffered event as well as before pulling a
/// new chunk, so a cancel cannot be outrun by frames that were already
/// parsed, and a terminal event drops whatever decoded behind it. Split
/// from [`CursorWire::stream`] so a fixture drives exactly the pipeline a
/// live turn runs, minus the socket.
fn events<S, C, E>(chunks: S, cancel: CancellationToken) -> BoxStream<'static, ProviderEvent>
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
                                    state.mapping.frame(&frame, &mut state.scratch);
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

/// Describes a failed read of the unary body, following the cause chain the
/// way the retry driver does for the request itself — `reqwest::Error`
/// alone says only that something failed, never why — and dropping the URL
/// first because a base URL may carry credentials. The driver owns every
/// failure up to the first byte; this covers the one read after it that the
/// unary RPC still makes whole.
fn transport(error: reqwest::Error) -> ProviderError {
    use std::fmt::Write as _;

    let error = error.without_url();
    let mut message = error.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&error);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }

    ProviderError::Transport(message)
}

/// Feeds a recorded body through the pipeline a live turn runs.
///
/// Delivering the whole body as one chunk is the worst case for
/// cancellation — every frame already parsed and waiting — which is exactly
/// what the cancel test wants to prove is still stoppable.
#[cfg(test)]
fn replay(body: Vec<u8>, cancel: CancellationToken) -> BoxStream<'static, ProviderEvent> {
    events(
        stream::iter([Ok::<Vec<u8>, std::convert::Infallible>(body)]),
        cancel,
    )
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use buffa::Message as _;
    use futures::StreamExt as _;
    use tokio_util::sync::CancellationToken;

    use super::{
        super::PROVIDERS, CursorProvider, CursorWire, DEFAULT_BASE_URL, ID, Provider as _,
        ProviderError, connect, proto,
    };
    use crate::{
        auth::{self, AuthError, OauthCredential, RefreshOauth},
        protocol::FinishReason,
        provider::ProviderEvent,
    };

    /// A renewal that must never run, for the cases that are about
    /// construction rather than about a token endpoint.
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

    /// A data frame holding one update, built with the real message types
    /// so the fold is driven by exactly what the server would send.
    fn framed(update: proto::Update) -> Vec<u8> {
        let message = proto::ServerMessage {
            interaction_update: buffa::MessageField::some(update),
            ..Default::default()
        };

        connect::envelope(&message.encode_to_vec())
    }

    fn text(delta: &str) -> proto::Update {
        proto::Update {
            text_delta: buffa::MessageField::some(proto::TextDelta::default().with_text(delta)),
            ..Default::default()
        }
    }

    fn turn_ended() -> proto::Update {
        proto::Update {
            turn_ended: buffa::MessageField::some(proto::TurnEnded::default()),
            ..Default::default()
        }
    }

    /// An EndStream frame carrying `payload`.
    fn end_stream(payload: &str) -> Vec<u8> {
        let mut frame = vec![0b0000_0010];
        frame.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("a test payload fits")
                .to_be_bytes(),
        );
        frame.extend_from_slice(payload.as_bytes());

        frame
    }

    #[test]
    fn ganja_calls_it_cursor_everywhere_the_wire_can_see() {
        assert_eq!(CursorProvider.id(), ID);
        assert_eq!(ID, "cursor");
        assert_eq!(
            ID,
            auth::cursor::PROVIDER_ID,
            "one constant, or a login stores under a name the provider does not read"
        );
        assert!(
            PROVIDERS.contains(&ID),
            "a provider nothing can select is a provider nobody has"
        );
    }

    #[test]
    fn the_endpoint_is_cursors_own_and_the_debug_holds_no_secret() {
        assert_eq!(DEFAULT_BASE_URL, "https://api2.cursor.sh");

        // Built through `at` at the same constant `from_stored` passes,
        // because `from_stored` reads whatever credential store the machine
        // running this suite really holds.
        let wire =
            CursorWire::at(DEFAULT_BASE_URL, Arc::new(NeverRenews)).expect("a client builds");
        let rendered = format!("{wire:?}");
        assert!(
            rendered.contains("Oauth") && rendered.contains("cursor"),
            "{rendered}"
        );
        assert!(
            rendered.contains("https://api2.cursor.sh"),
            "the endpoint is what tells one wire from another: {rendered}"
        );

        // The selectable identity stays a bare name: nothing to leak, and
        // nothing read at construction.
        assert_eq!(format!("{CursorProvider:?}"), "CursorProvider");
    }

    /// The endpoint is not exempt from the rule every other base URL is held
    /// to just because the credential arrived as a token rather than a key.
    #[test]
    fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
        let refused = CursorWire::at("http://api2.cursor.sh", Arc::new(NeverRenews))
            .expect_err("plain http to a public host puts the token on the wire in the clear");
        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );
        assert!(
            CursorWire::at("http://127.0.0.1:4096", Arc::new(NeverRenews)).is_ok(),
            "loopback never reaches a network, which is what a test depends on"
        );
    }

    /// The whole reason this fold exists: an event reaches the session while
    /// the response body is still open. A fold that buffered until the end
    /// of the body would leave the first `next()` waiting on a channel
    /// nothing has closed, which the timeout turns into a readable failure.
    #[tokio::test]
    async fn a_delta_is_handed_over_while_the_body_is_still_open() {
        let (sender, receiver) =
            futures::channel::mpsc::unbounded::<Result<Vec<u8>, std::convert::Infallible>>();
        let mut stream = super::events(receiver, CancellationToken::new());

        sender
            .unbounded_send(Ok(framed(text("Hello"))))
            .expect("the body is open");
        let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("the delta must arrive before the body ends");
        assert_eq!(
            first,
            Some(ProviderEvent::TextDelta("Hello".to_owned())),
            "the first frame's event, with the rest of the body unwritten"
        );

        let mut rest = framed(text(" world"));
        rest.extend(framed(turn_ended()));
        rest.extend(end_stream("{}"));
        sender.unbounded_send(Ok(rest)).expect("the body is open");
        drop(sender);

        let tail: Vec<ProviderEvent> = stream.collect().await;
        assert_eq!(
            tail,
            vec![
                ProviderEvent::TextDelta(" world".to_owned()),
                ProviderEvent::Finish(FinishReason::Completed),
            ]
        );
    }

    /// The other wires' cancellation contract, replicated from
    /// `anthropic`'s test of the same name: the whole transcript delivered
    /// as one chunk is the worst case — every frame already parsed and
    /// waiting — and the cancel still wins.
    #[tokio::test]
    async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
        let mut body = framed(text("Hello"));
        body.extend(framed(text(" world")));
        body.extend(framed(turn_ended()));
        body.extend(end_stream("{}"));

        let cancel = CancellationToken::new();
        let mut stream = super::replay(body, cancel.clone());

        assert_eq!(
            stream.next().await,
            Some(ProviderEvent::TextDelta("Hello".to_owned()))
        );
        cancel.cancel();

        let rest: Vec<ProviderEvent> = stream.collect().await;
        assert!(
            rest.is_empty(),
            "a cancelled stream ends; the engine is what calls that Cancelled, and it \
             cannot if a Finish or a Failed arrives: {rest:?}"
        );
    }

    /// A terminal event drops whatever decoded behind it — the shared
    /// folds' contract — so a body talking past its EndStream frame ends on
    /// the verdict rather than on the splitter's complaint about the
    /// trailing bytes.
    #[tokio::test]
    async fn nothing_follows_the_streams_verdict() {
        let mut body = framed(text("done"));
        body.extend(end_stream("{}"));
        body.push(0x00);

        let events: Vec<ProviderEvent> = super::replay(body, CancellationToken::new())
            .collect()
            .await;
        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("done".to_owned()),
                ProviderEvent::Finish(FinishReason::Completed),
            ]
        );
    }

    /// The admitted runtime is real, not merely named: a generated message
    /// round-trips through `buffa`'s encode/decode. This is what makes the
    /// dependency reach the lock (so `cargo deny` audits its license) and
    /// the live listing decode against the same version.
    #[test]
    fn the_admitted_protobuf_runtime_round_trips_a_generated_message() {
        use buffa::Message as _;

        let entry = super::proto::ModelEntry::default()
            .with_model_id("gpt-5.3-codex")
            .with_display_name("Codex 5.3");
        let bytes = entry.encode_to_vec();
        let decoded = super::proto::ModelEntry::decode_from_slice(&bytes)
            .expect("a message buffa encoded decodes");

        assert_eq!(decoded.model_id.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(decoded.display_name.as_deref(), Some("Codex 5.3"));
    }

    /// The checked-in generated code still matches its `.proto`:
    /// regenerating with the same remote plugin must produce byte-identical
    /// output. A drift here means somebody edited the `@generated` file by
    /// hand or changed the `.proto` without regenerating — either way the
    /// source of truth and the compiled code have diverged.
    ///
    /// Skipped rather than failed when `buf` is absent: the drift check is a
    /// developer-machine guard, and the workspace deliberately keeps `buf`
    /// and `protoc` out of CI (the generated code is checked in for exactly
    /// that reason). CI proves the code compiles and round-trips; this
    /// proves it was not hand-edited, on a machine that can regenerate.
    #[test]
    fn the_checked_in_generated_code_matches_the_proto() {
        use std::process::Command;

        let crate_dir = env!("CARGO_MANIFEST_DIR");
        if Command::new("buf").arg("--version").output().is_err() {
            eprintln!("skipping the proto drift check: `buf` is not on PATH");
            return;
        }

        let generated =
            std::path::Path::new(crate_dir).join("src/provider/cursor/ganja.cursor.v1.rs");
        let before = std::fs::read_to_string(&generated).expect("the generated file is present");

        let status = Command::new("buf")
            .arg("generate")
            .current_dir(crate_dir)
            .status()
            .expect("buf generate runs");
        assert!(status.success(), "buf generate failed");

        let after = std::fs::read_to_string(&generated).expect("the generated file is present");
        assert_eq!(
            before, after,
            "the checked-in cursor protobuf code has drifted from cursor.proto; \
             run `buf generate` in crates/ganja-provider and commit the result"
        );
    }
}
