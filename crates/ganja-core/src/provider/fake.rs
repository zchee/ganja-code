//! The provider every demo and end-to-end test runs against.

use std::time::Duration;

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{FinishReason, Part, Usage},
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
pub const ID: &str = "fake";

/// Model identifier this provider reports. It differs from [`ID`] so that a
/// message envelope shows which of the two it recorded.
pub const MODEL: &str = "canned";

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

/// Streams [`REPLY`] one word at a time, ignoring what was asked.
#[derive(Clone, Debug)]
pub struct FakeProvider {
    chunks: Vec<String>,
    cadence: Duration,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new(REPLY, CADENCE)
    }
}

impl FakeProvider {
    /// Builds a provider that emits `reply` one word per `cadence`.
    #[must_use]
    pub fn new(reply: &str, cadence: Duration) -> Self {
        Self {
            chunks: split_into_chunks(reply),
            cadence,
        }
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let usage = Usage {
            input_tokens: count_words(&request),
            output_tokens: u64::try_from(self.chunks.len()).unwrap_or(u64::MAX),
            ..Usage::default()
        };
        let events: Vec<ProviderEvent> = self
            .chunks
            .iter()
            .cloned()
            .map(ProviderEvent::TextDelta)
            .chain([
                ProviderEvent::Usage(usage),
                ProviderEvent::Finish(FinishReason::Completed),
            ])
            .collect();
        let cadence = self.cadence;

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
mod tests {
    use std::time::{Duration, Instant};

    use futures::StreamExt as _;
    use tokio_util::sync::CancellationToken;

    use super::{FakeProvider, ID, MODEL, REPLY, split_into_chunks};
    use crate::{
        protocol::{FinishReason, Message, Usage},
        provider::{ChatRequest, Provider as _, ProviderEvent},
    };

    fn request(prompt: &str) -> ChatRequest {
        ChatRequest {
            model: MODEL.to_owned(),
            system: None,
            messages: vec![Message::user(prompt)],
        }
    }

    #[test]
    fn chunks_concatenate_back_into_the_reply() {
        assert_eq!(split_into_chunks(REPLY).concat(), REPLY);
    }

    #[test]
    fn the_reply_opens_with_a_word_a_pty_test_can_wait_for() {
        let chunks = split_into_chunks(REPLY);

        assert_eq!(chunks.first().map(String::as_str), Some("Acknowledged. "));
    }

    #[test]
    fn blank_input_produces_no_chunks() {
        assert!(split_into_chunks("").is_empty());
    }

    #[tokio::test]
    async fn the_stream_paces_itself_and_reports_what_it_spent() {
        let cadence = Duration::from_millis(2);
        let provider = FakeProvider::new("one two three", cadence);
        assert_eq!(provider.id(), ID);

        let started = Instant::now();
        let events: Vec<ProviderEvent> = provider
            .stream(request("count to three"), CancellationToken::new())
            .await
            .expect("the fake provider always answers")
            .collect()
            .await;

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(text, "one two three");
        assert_eq!(
            events.last(),
            Some(&ProviderEvent::Finish(FinishReason::Completed)),
            "a turn has to end with a finish, got {events:?}"
        );
        assert!(
            events.contains(&ProviderEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 3,
                ..Usage::default()
            })),
            "usage should count the prompt and the reply, got {events:?}"
        );
        assert!(
            started.elapsed() >= cadence * 3,
            "three fragments should take at least three cadences, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_cancelled_stream_stops_yielding() {
        let provider = FakeProvider::new("one two three", Duration::from_millis(5));
        let cancel = CancellationToken::new();
        let mut events = provider
            .stream(request("count to three"), cancel.clone())
            .await
            .expect("the fake provider always answers");

        assert_eq!(
            events.next().await,
            Some(ProviderEvent::TextDelta("one ".to_owned()))
        );
        cancel.cancel();

        assert_eq!(events.next().await, None, "a cancelled stream should end");
    }
}
