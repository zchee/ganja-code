//! The provider every demo and end-to-end test runs against.

use std::time::Duration;

use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};

use crate::provider::Provider;

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
pub const ID: &str = "fake";

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

/// Streams [`REPLY`] one word at a time, ignoring the prompt.
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

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        ID
    }

    fn stream(&self, _prompt: &str) -> BoxStream<'static, String> {
        let cadence = self.cadence;

        stream::iter(self.chunks.clone())
            .then(move |chunk| async move {
                tokio::time::sleep(cadence).await;
                chunk
            })
            .boxed()
    }
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

    use super::{FakeProvider, ID, REPLY, split_into_chunks};
    use crate::provider::Provider as _;

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
    async fn the_stream_paces_itself_and_reproduces_the_reply() {
        let cadence = Duration::from_millis(2);
        let provider = FakeProvider::new("one two three", cadence);
        assert_eq!(provider.id(), ID);

        let started = Instant::now();
        let fragments: Vec<String> = provider.stream("ignored").collect().await;

        assert_eq!(fragments.concat(), "one two three");
        assert!(
            started.elapsed() >= cadence * 3,
            "three fragments should take at least three cadences, took {:?}",
            started.elapsed()
        );
    }
}
