//! Sources of assistant text.
//!
//! A provider turns a [`ChatRequest`] into a stream of [`ProviderEvent`]s; the
//! engine is what maps those onto the protocol frontends see. Three ship:
//! [`FakeProvider`] for demos and tests, [`AnthropicProvider`] for the Messages
//! API, and [`OpenAiProvider`] for anything speaking OpenAI chat completions.
//!
//! Both HTTP providers share the same shape — build a request, retry it while
//! it has not started answering, split the `text/event-stream` body into
//! [`sse::Frame`]s, and map those onto events — so everything except the
//! mapping lives here.
//!
//! Failures are reported in one of two ways, and never as a completed turn. A
//! request that never starts streaming fails the call to [`Provider::stream`];
//! one that dies mid-stream yields [`ProviderEvent::Failed`]. The engine turns
//! both into a `Failed` finish carrying the message.

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
use tokio_util::sync::CancellationToken;

use crate::{
    auth, catalog,
    protocol::{FinishReason, Message, Usage},
    provider::sse::Frame,
};

pub mod anthropic;
pub mod fake;
pub mod openai;
pub mod retry;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use fake::FakeProvider;
pub use openai::OpenAiProvider;

/// Environment variable naming the provider a session talks to.
pub const PROVIDER_ENV: &str = "GANJA_PROVIDER";

/// Environment variable overriding the model a session asks for.
pub const MODEL_ENV: &str = "GANJA_MODEL";

/// Every value [`PROVIDER_ENV`] accepts.
pub const PROVIDERS: [&str; 3] = [anthropic::ID, openai::ID, fake::ID];

/// One request to a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatRequest {
    /// Model identifier, spelled the way the provider expects it on the wire.
    pub model: String,
    /// System prompt. P2 has nothing to put here; P5 fills it from `AGENTS.md`
    /// and the agent definitions.
    pub system: Option<String>,
    /// The conversation so far, oldest first, ending with the message the user
    /// just sent.
    pub messages: Vec<Message>,
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

/// An API key.
///
/// The only way to read one is [`ApiKey::expose`], which exists so that a
/// grep for it finds every place a credential leaves the type. Everything else
/// — [`fmt::Debug`], and therefore every `tracing` field that renders a
/// provider — sees a placeholder.
#[derive(Clone, PartialEq, Eq)]
struct ApiKey(String);

impl ApiKey {
    /// Wraps a credential, rejecting a blank one so that an exported-but-empty
    /// variable fails at startup rather than as a 401 mid-turn.
    fn new(key: impl Into<String>) -> Option<Self> {
        let key = key.into();

        (!key.trim().is_empty()).then_some(Self(key))
    }

    /// The credential itself, for putting on the wire.
    fn expose(&self) -> &str {
        &self.0
    }

    /// Replaces the credential with a placeholder wherever it appears in
    /// `text`, so that a provider echoing back the key it rejected cannot put
    /// it in an error message or a log line.
    fn redact(&self, text: &str) -> String {
        text.replace(&self.0, "[redacted]")
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([redacted])")
    }
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
    key: &ApiKey,
    cancel: CancellationToken,
    mapper: M,
) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
    let response = match retry::send(client, request, key, &cancel).await {
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
/// exported key still overrides a stored one for a single run.
///
/// A store that could not be read is [`Err`], not [`Ok(None)`]: "you have no
/// credential" and "you have one and it was refused" need different things
/// from the person reading the message, and only the second can say what to
/// fix. Reporting it here rather than logging it is what gets the reason in
/// front of someone who is looking at a terminal, not a log file.
fn credential_for(provider_id: &str) -> Result<Option<ApiKey>, ProviderError> {
    match auth::credential_for(provider_id) {
        Ok(credential) => Ok(credential.and_then(|credential| ApiKey::new(credential.api_key))),
        // Every `AuthError` names the file and the command that repairs it
        // while quoting nothing out of it — the parse failure deliberately
        // throws away serde's message because it would echo the value — so the
        // whole taxonomy is safe to put in front of a user verbatim.
        Err(error) => Err(ProviderError::Auth(error.to_string())),
    }
}

/// The key for `provider_id`, or the error a startup should die on.
fn require_credential(provider_id: &str, variable: &str) -> Result<ApiKey, ProviderError> {
    credential_for(provider_id)?.ok_or_else(|| {
        ProviderError::Auth(format!(
            "{variable} is unset; export it or run `ganja auth login`"
        ))
    })
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
/// # Errors
///
/// Returns [`SelectionError`] when the variable names a provider this build
/// does not have, or names one whose credentials are missing. Both fail here,
/// before the terminal is put into raw mode, so that the message is readable.
pub fn from_env() -> Result<Selection, SelectionError> {
    let requested = match env::var(PROVIDER_ENV) {
        Ok(requested) => requested,
        Err(VarError::NotUnicode(requested)) => {
            return Err(SelectionError::Unknown {
                requested: requested.to_string_lossy().into_owned(),
            });
        }
        Err(VarError::NotPresent) => {
            return Ok(Selection {
                provider: Arc::new(FakeProvider::default()),
                model: setting(MODEL_ENV).unwrap_or_else(|| fake::MODEL.to_owned()),
                notice: Some(format!(
                    "{PROVIDER_ENV} is unset - replying from the built-in {} provider",
                    fake::ID
                )),
            });
        }
    };

    let provider: Arc<dyn Provider> = match requested.as_str() {
        fake::ID => Arc::new(FakeProvider::default()),
        anthropic::ID => Arc::new(AnthropicProvider::from_env()?),
        openai::ID => Arc::new(OpenAiProvider::from_env()?),
        _ => return Err(SelectionError::Unknown { requested }),
    };

    // The catalog owns the defaults, so a session that names no model still
    // asks for one whose context window and price this build knows.
    let model = match setting(MODEL_ENV) {
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

    use super::{ApiKey, Mapper, ProviderError, ProviderEvent, events, sse::Frame};
    use crate::protocol::FinishReason;

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
    fn a_key_never_renders_itself() {
        let key = ApiKey::new("sk-test-canary-XYZ").expect("a non-blank key");

        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
        assert_eq!(
            key.redact("rejected sk-test-canary-XYZ, sorry"),
            "rejected [redacted], sorry"
        );
        assert_eq!(key.expose(), "sk-test-canary-XYZ");
        assert!(ApiKey::new("   ").is_none(), "a blank key is not a key");
        assert!(ApiKey::new("").is_none());
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
