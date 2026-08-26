//! The provider every demo and end-to-end test runs against.
//!
//! Unscripted it streams one canned answer, ignoring what was asked. That is
//! enough to exercise a terminal but not an agent loop, so a session can hand
//! it a script instead: [`SCRIPT_ENV`] names a JSON file, one entry per model
//! request, and the provider plays entry *n* for request *n*.
//!
//! ```json
//! {
//!   "cadence_ms": 20,
//!   "turns": [
//!     {
//!       "thinking": "The file is the only thing worth reading first.",
//!       "text": "Reading the file first.",
//!       "tool_calls": [{"name": "read", "args": {"filePath": "src/main.rs"}}]
//!     },
//!     {"text": "It holds a main function and nothing else."}
//!   ]
//! }
//! ```
//!
//! Every key of a turn is optional: a turn with no `thinking` shows no
//! thought, one with no `text` streams nothing before its calls, and one with
//! no `tool_calls` is a plain reply. `args` defaults to `{}`. `cadence_ms`
//! overrides the delay between fragments for the whole script, and an absent
//! one keeps the provider's own.
//!
//! A turn streams its thinking one word at a time — as the readable kind,
//! [`ProviderEvent::ReasoningDelta`], the display-only part every frontend
//! draws and no wire replays, which is what a demo of the transcript's
//! thinking block needs and a real provider charges for — then its text the
//! same way, then each of its calls as a
//! start, at least two argument fragments, and an end — the fragmenting is
//! deliberate, so that anything consuming these events has to buffer the
//! arguments rather than parse the first one it sees. Call identifiers are
//! `call_1`, `call_2`, and so on across the whole script, so a transcript can
//! be compared byte for byte between runs. Requests past the last turn stream
//! [`EXHAUSTED`], which keeps a demo that over-runs its script from looking
//! like a hang.
//!
//! A script that cannot be read or cannot be parsed fails the turn it was asked
//! for, loudly, with the reason: falling back to the canned reply would leave a
//! demo silently proving nothing.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{FinishReason, Part, Usage},
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent, setting},
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
pub const ID: &str = "fake";

/// Model identifier this provider reports. It differs from [`ID`] so that a
/// message envelope shows which of the two it recorded.
pub const MODEL: &str = "canned";

/// Environment variable naming a JSON script for this provider to play, in
/// place of the canned reply. The module documentation carries the format.
pub const SCRIPT_ENV: &str = "GANJA_FAKE_SCRIPT";

/// What a request past the end of a script is answered with.
pub const EXHAUSTED: &str = "The script has no more turns.";

/// Delay between fragments, close enough to a real model's token cadence that
/// the render loop's coalescing is actually exercised.
const CADENCE: Duration = Duration::from_millis(20);

/// The canned answer. It opens with a word that appears nowhere else in the UI
/// so that a pty test can wait for it without matching chrome.
const REPLY: &str = "\
Acknowledged. This reply comes from the built-in fake provider, which streams a \
canned answer one word at a time so the terminal can be exercised end to end \
without a network connection or an API key.

Every fragment travels the path a real model response will take: the provider \
yields text, the engine queues it on a bounded channel that no consumer can \
overflow, and the render loop coalesces the arrivals into at most one frame per \
sixteen milliseconds.

Press Esc while this is streaming and the turn stops inside a tenth of a second. \
Scroll with the wheel, PageUp, or PageDown; press End to follow the tail again.";

/// Streams `REPLY` one word at a time, or plays the script it was given.
#[derive(Clone, Debug)]
pub struct FakeProvider {
    chunks: Vec<String>,
    cadence: Duration,
    /// Script to play instead of [`chunks`](Self::chunks), when one was named.
    script: Option<PathBuf>,
    /// How many turns have been asked for, which is what selects the entry a
    /// script plays. Shared with every clone, so that passing a provider around
    /// does not restart its script.
    requests: Arc<AtomicUsize>,
    /// Every request asked of this provider, oldest first, shared with every
    /// clone. This is the fake's "sent body": it builds no HTTP request, so
    /// what a test reads to see what would have gone on the wire — the
    /// messages, the tools, an effort's option map — is the [`ChatRequest`]
    /// itself, recorded verbatim.
    recorded: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

impl Default for FakeProvider {
    /// Plays the script [`SCRIPT_ENV`] names, or `REPLY` when it names none.
    fn default() -> Self {
        Self {
            script: setting(SCRIPT_ENV).map(PathBuf::from),
            ..Self::new(REPLY, CADENCE)
        }
    }
}

impl FakeProvider {
    /// Builds a provider that emits `reply` one word per `cadence`.
    ///
    /// The environment is not consulted: a provider built this way answers with
    /// `reply` whatever [`SCRIPT_ENV`] says, which is what keeps a test that
    /// asks for canned text from depending on the environment it runs in.
    #[must_use]
    pub fn new(reply: &str, cadence: Duration) -> Self {
        Self {
            chunks: split_into_chunks(reply),
            cadence,
            script: None,
            requests: Arc::new(AtomicUsize::new(0)),
            recorded: Arc::default(),
        }
    }

    /// Every request this provider has been asked so far, oldest first.
    #[must_use]
    pub fn recorded(&self) -> Vec<ChatRequest> {
        self.recorded
            .lock()
            .expect("the request log is never poisoned")
            .clone()
    }

    /// Plays the script at `path` instead of the canned reply.
    #[must_use]
    pub fn with_script(mut self, path: impl Into<PathBuf>) -> Self {
        self.script = Some(path.into());
        self
    }

    /// The canned reply, as the events one turn of it streams.
    fn canned(&self, request: &ChatRequest) -> Vec<ProviderEvent> {
        let usage = Usage {
            input_tokens: count_words(request),
            output_tokens: u64::try_from(self.chunks.len()).unwrap_or(u64::MAX),
            ..Usage::default()
        };

        self.chunks
            .iter()
            .cloned()
            .map(ProviderEvent::TextDelta)
            .chain([
                ProviderEvent::Usage(usage),
                ProviderEvent::Finish(FinishReason::Completed),
            ])
            .collect()
    }

    /// The next turn of the script at `path`, as the events it streams.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the script cannot be read or cannot be
    /// understood. Nothing falls back to the canned reply: a demo driven by a
    /// script it silently ignored proves nothing, and the message is what says
    /// which file to go and fix.
    async fn scripted(
        &self,
        path: &Path,
        request: &ChatRequest,
    ) -> Result<(Vec<ProviderEvent>, Duration), ProviderError> {
        let script = Script::read(path).await?;
        // Counted after the read, so a script that has to be repaired mid-demo
        // does not consume the turn it failed on.
        let index = self.requests.fetch_add(1, Ordering::Relaxed);

        let (thinking, text, calls) = match script.turns.get(index) {
            Some(turn) => (
                turn.thinking.as_str(),
                turn.text.as_str(),
                turn.tool_calls.as_slice(),
            ),
            None => ("", EXHAUSTED, &[][..]),
        };

        // The thought first, as a model streams it; counted towards nothing,
        // the way the engine's own meter counts readable thinking.
        let mut events: Vec<ProviderEvent> = split_into_chunks(thinking)
            .into_iter()
            .map(ProviderEvent::ReasoningDelta)
            .collect();
        let replies: Vec<ProviderEvent> = split_into_chunks(text)
            .into_iter()
            .map(ProviderEvent::TextDelta)
            .collect();
        let fragments = replies.len();
        events.extend(replies);

        // Numbered across the whole script rather than within the turn, so that
        // no two calls in one session share an id — the engine keys a tool part
        // on it, and a turn is several requests once tools are in play. `take`
        // rather than a slice, because a request past the end of the script has
        // an index past the end of the turns.
        let first = 1 + script
            .turns
            .iter()
            .take(index)
            .map(|turn| turn.tool_calls.len())
            .sum::<usize>();

        for (offset, call) in calls.iter().enumerate() {
            let id = format!("call_{}", first + offset);

            events.push(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: call.name.clone(),
            });
            events.extend(
                fragmented(&call.args).map(|json| ProviderEvent::ToolCallDelta {
                    id: id.clone(),
                    json,
                }),
            );
            events.push(ProviderEvent::ToolCallEnd { id });
        }

        events.push(ProviderEvent::Usage(Usage {
            input_tokens: count_words(request),
            output_tokens: u64::try_from(fragments).unwrap_or(u64::MAX),
            ..Usage::default()
        }));
        events.push(ProviderEvent::Finish(FinishReason::Completed));

        Ok((
            events,
            script
                .cadence_ms
                .map_or(self.cadence, Duration::from_millis),
        ))
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        ID
    }

    /// Everything: this provider reads no request, so refusing a mime would
    /// only put a degradation notice in front of a demo that was proving the
    /// attachment path works.
    fn accepts_attachment(&self, _mime: &str) -> bool {
        true
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let (events, cadence) = match &self.script {
            Some(path) => self.scripted(path, &request).await?,
            None => (self.canned(&request), self.cadence),
        };
        self.recorded
            .lock()
            .expect("the request log is never poisoned")
            .push(request);

        // Ending the stream on cancel is what a real provider does when its
        // response body is dropped; the engine stops the turn either way, but
        // this keeps the fake from outliving it.
        Ok(stream::unfold(events.into_iter(), move |mut events| {
            let cancel = cancel.clone();

            async move {
                let event = events.next()?;

                tokio::select! {
                    () = cancel.cancelled() => None,
                    () = tokio::time::sleep(cadence) => Some((event, events)),
                }
            }
        })
        .boxed())
    }
}

/// A scripted session, as the file [`SCRIPT_ENV`] names holds it.
///
/// Unknown fields are refused rather than ignored: a script is written by hand,
/// and a mistyped `tool_call` that quietly played as a plain reply would look
/// exactly like a bug in whatever is being demonstrated.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    /// Delay between fragments, in milliseconds. Absent keeps the provider's.
    #[serde(default)]
    cadence_ms: Option<u64>,
    /// One entry per model request, played in order.
    turns: Vec<ScriptTurn>,
}

/// One request's worth of a script.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptTurn {
    /// Streamed one word at a time before the text, as thinking a person can
    /// read.
    #[serde(default)]
    thinking: String,
    /// Streamed one word at a time before any call.
    #[serde(default)]
    text: String,
    /// The calls the turn makes, in order.
    #[serde(default)]
    tool_calls: Vec<ScriptCall>,
}

/// One call in a script.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptCall {
    /// Tool to call, by registry id.
    name: String,
    /// Arguments, streamed as at least two fragments.
    #[serde(default = "no_args")]
    args: Value,
}

/// The arguments a call that names none is given.
fn no_args() -> Value {
    Value::Object(serde_json::Map::new())
}

impl Script {
    /// Reads and parses the script at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when the file cannot be read and
    /// [`ProviderError::Parse`] when it is not a script. Both name the file, so
    /// that a demo that fails says where to look.
    async fn read(path: &Path) -> Result<Self, ProviderError> {
        let source = tokio::fs::read_to_string(path).await.map_err(|error| {
            ProviderError::Transport(format!(
                "{SCRIPT_ENV} names {}, which cannot be read: {error}",
                path.display()
            ))
        })?;

        serde_json::from_str(&source).map_err(|error| {
            ProviderError::Parse(format!(
                "{SCRIPT_ENV} names {}, which is not a script: {error}",
                path.display()
            ))
        })
    }
}

/// Splits a call's arguments into the fragments they stream as.
///
/// Always more than one where there is more than one character to split, which
/// is the point: a consumer that parses the first fragment it is handed instead
/// of buffering the call has to fail its tests here rather than against a real
/// provider.
fn fragmented(args: &Value) -> impl Iterator<Item = String> {
    let json = serde_json::to_string(args).expect("a serde_json::Value always serializes");
    let middle = json
        .char_indices()
        .nth(json.chars().count() / 2)
        .map_or(json.len(), |(offset, _)| offset);

    if middle == 0 {
        return vec![json].into_iter();
    }

    vec![json[..middle].to_owned(), json[middle..].to_owned()].into_iter()
}

/// Counts the words a request carries, which is the only honest "input size" a
/// provider that never leaves the process can report.
fn count_words(request: &ChatRequest) -> u64 {
    let words = request
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(Part::as_text)
        .flat_map(str::split_whitespace)
        .count();

    u64::try_from(words).unwrap_or(u64::MAX)
}

/// Splits `reply` into word-plus-trailing-whitespace fragments, which
/// concatenate back to `reply` exactly.
fn split_into_chunks(reply: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = reply;

    while !rest.is_empty() {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let chunk_end = rest[word_end..]
            .find(|character: char| !character.is_whitespace())
            .map_or(rest.len(), |offset| word_end + offset);

        chunks.push(rest[..chunk_end].to_owned());
        rest = &rest[chunk_end..];
    }

    chunks
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod tests;
