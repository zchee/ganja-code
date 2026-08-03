//! A Server-Sent Events frame splitter.
//!
//! Both HTTP providers read `text/event-stream` bodies, and neither of them
//! gets to assume that a frame arrives in one chunk: `reqwest` hands over
//! whatever the socket produced, which splits fields, lines, and multi-byte
//! characters wherever it likes. [`Decoder`] absorbs that by buffering until a
//! line terminator is in hand, so the frames it emits are identical however the
//! bytes were carved up.
//!
//! The parser follows the WHATWG event-stream rules — `\r\n`, `\n`, and a lone
//! `\r` all end a line; a leading `:` marks a comment; a field with no colon
//! has an empty value; one leading space is stripped from a value; `data:`
//! lines accumulate newline-separated — and is deliberately forgiving about the
//! rest. Fields it does not know (`id`, `retry`, whatever a provider invents)
//! are ignored rather than rejected, which is what keeps an unfamiliar event
//! type from ending a turn.
//!
//! `eventsource-stream` would have covered this; the port rejected it as
//! unmaintained, so this is the ~100 lines that replace it.

use std::collections::VecDeque;

use futures::{Stream, StreamExt as _, stream};

/// One dispatched event-stream frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// The `event:` field, absent when the frame carried none. Anthropic names
    /// every frame; OpenAI names none.
    pub event: Option<String>,
    /// The `data:` field(s), joined with newlines and with the trailing one
    /// removed.
    pub data: String,
}

impl Frame {
    /// The event name, or `""` for a frame that carried none, so that a mapper
    /// can `match` on one `&str` instead of an `Option`.
    #[must_use]
    pub fn event(&self) -> &str {
        self.event.as_deref().unwrap_or_default()
    }
}

/// Turns byte chunks into [`Frame`]s, remembering whatever the last chunk left
/// half-finished.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    /// Bytes past the last line terminator, which is at most one partial line.
    buffer: Vec<u8>,
    /// `event:` of the frame being assembled.
    event: Option<String>,
    /// `data:` lines of the frame being assembled, newline-terminated.
    data: String,
    /// Whether the frame being assembled has seen a `data:` field. A frame
    /// without one is not dispatched, so this cannot be inferred from `data`.
    has_data: bool,
}

impl Decoder {
    /// Feeds `chunk`, appending every frame it completed to `frames`.
    ///
    /// Whatever the chunk left unterminated stays buffered for the next call.
    /// A stream that ends there — a dropped connection, a body truncated by a
    /// proxy — simply loses it, which is the honest reading: the provider never
    /// said the frame was complete.
    pub fn push<O: Extend<Frame>>(&mut self, chunk: &[u8], frames: &mut O) {
        self.buffer.extend_from_slice(chunk);

        let mut start = 0;
        while let Some(offset) = self.buffer[start..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            let end = start + offset;

            // A `\r` with nothing after it may yet turn out to be the first
            // half of a `\r\n` that the next chunk finishes.
            if self.buffer[end] == b'\r' && end + 1 == self.buffer.len() {
                break;
            }

            let terminator = if self.buffer[end] == b'\r' && self.buffer[end + 1] == b'\n' {
                2
            } else {
                1
            };
            // Lossy because a provider that sends invalid UTF-8 has already
            // broken its own contract, and losing the turn over it would be
            // worse than rendering a replacement character.
            let line = String::from_utf8_lossy(&self.buffer[start..end]).into_owned();
            self.line(&line, frames);
            start = end + terminator;
        }

        self.buffer.drain(..start);
    }

    /// Applies one complete line.
    fn line<O: Extend<Frame>>(&mut self, line: &str, frames: &mut O) {
        if line.is_empty() {
            self.dispatch(frames);
            return;
        }
        if line.starts_with(':') {
            return;
        }

        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            (field, value.strip_prefix(' ').unwrap_or(value))
        });

        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            unknown => tracing::trace!(field = unknown, "ignoring an event-stream field"),
        }
    }

    /// Emits the frame a blank line just closed, if there is one.
    fn dispatch<O: Extend<Frame>>(&mut self, frames: &mut O) {
        if !self.has_data {
            // A frame with no data is not dispatched, and takes its event name
            // down with it rather than leaking into the next one.
            self.event = None;
            self.data.clear();
            return;
        }

        let mut data = std::mem::take(&mut self.data);
        data.pop();
        self.has_data = false;

        frames.extend([Frame {
            event: self.event.take(),
            data,
        }]);
    }
}

/// Splits `chunks` into frames.
///
/// Generic over the chunk type so that the same pipeline serves
/// [`reqwest::Response::bytes_stream`] and a fixture read off disk; errors pass
/// through untouched, because only the caller knows whether a truncated stream
/// is fatal.
pub fn frames<S, C, E>(chunks: S) -> impl Stream<Item = Result<Frame, E>> + Send
where
    S: Stream<Item = Result<C, E>> + Send + Unpin + 'static,
    C: AsRef<[u8]> + Send,
    E: Send,
{
    /// Everything the fold carries between polls.
    struct State<S> {
        chunks: S,
        decoder: Decoder,
        ready: VecDeque<Frame>,
        done: bool,
    }

    stream::unfold(
        State {
            chunks,
            decoder: Decoder::default(),
            ready: VecDeque::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(frame) = state.ready.pop_front() {
                    return Some((Ok(frame), state));
                }
                if state.done {
                    return None;
                }

                match state.chunks.next().await {
                    Some(Ok(chunk)) => state.decoder.push(chunk.as_ref(), &mut state.ready),
                    Some(Err(error)) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                    None => return None,
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::{StreamExt as _, stream};

    use super::{Decoder, Frame, frames};

    /// Decodes `input` in one go.
    fn decode(input: &str) -> Vec<Frame> {
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        decoder.push(input.as_bytes(), &mut frames);

        frames
    }

    /// Decodes `input` one byte at a time, which is the worst chunking a
    /// socket can inflict.
    fn decode_byte_by_byte(input: &str) -> Vec<Frame> {
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();

        for byte in input.as_bytes() {
            decoder.push(&[*byte], &mut frames);
        }

        frames
    }

    fn frame(event: Option<&str>, data: &str) -> Frame {
        Frame {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn the_shapes_a_provider_can_send_all_decode() {
        let cases: Vec<(&str, &str, Vec<Frame>)> = vec![
            ("empty input", "", vec![]),
            (
                "a named frame",
                "event: ping\ndata: {}\n\n",
                vec![frame(Some("ping"), "{}")],
            ),
            (
                "an unnamed frame, which is all OpenAI sends",
                "data: {\"a\":1}\n\n",
                vec![frame(None, "{\"a\":1}")],
            ),
            (
                "crlf terminators",
                "event: ping\r\ndata: {}\r\n\r\n",
                vec![frame(Some("ping"), "{}")],
            ),
            (
                // A `\r` that ends the input is held rather than dispatched:
                // mid-stream it could still become a `\r\n`, and at the end of
                // a body an unterminated frame is dropped anyway.
                "a lone cr terminator",
                "data: one\r\rdata: two\n\n",
                vec![frame(None, "one"), frame(None, "two")],
            ),
            (
                "multi-line data joins with newlines",
                "data: one\ndata: two\ndata: three\n\n",
                vec![frame(None, "one\ntwo\nthree")],
            ),
            (
                "an empty data line is content, not a terminator",
                "data: one\ndata:\ndata: two\n\n",
                vec![frame(None, "one\n\ntwo")],
            ),
            (
                "comments and unknown fields are ignored",
                ": keep-alive\nid: 7\nretry: 1000\nfuture: whatever\ndata: hi\n\n",
                vec![frame(None, "hi")],
            ),
            (
                "a field with no colon has an empty value",
                "data\n\n",
                vec![frame(None, "")],
            ),
            (
                "exactly one leading space is stripped",
                "data:  padded\n\n",
                vec![frame(None, " padded")],
            ),
            (
                "a frame with no data is not dispatched",
                "event: ping\n\ndata: hi\n\n",
                vec![frame(None, "hi")],
            ),
            (
                "back-to-back frames",
                "event: a\ndata: 1\n\nevent: b\ndata: 2\n\n",
                vec![frame(Some("a"), "1"), frame(Some("b"), "2")],
            ),
            (
                "an unterminated final frame is dropped",
                "data: kept\n\ndata: lost\n",
                vec![frame(None, "kept")],
            ),
            (
                "a blank line before anything dispatches nothing",
                "\n\n\ndata: hi\n\n",
                vec![frame(None, "hi")],
            ),
        ];

        for (name, input, expected) in cases {
            assert_eq!(decode(input), expected, "{name}: whole-input decode");
            assert_eq!(
                decode_byte_by_byte(input),
                expected,
                "{name}: byte-by-byte decode"
            );
        }
    }

    #[test]
    fn a_transcript_decodes_the_same_however_it_is_chunked() {
        let transcript = include_str!("../../tests/fixtures/anthropic_happy_path.sse");
        let whole = decode(transcript);

        assert!(!whole.is_empty(), "the fixture should carry frames");
        assert_eq!(
            decode_byte_by_byte(transcript),
            whole,
            "chunking must not change what a transcript decodes to"
        );

        // Every split point, so no boundary is special.
        for split in 0..transcript.len() {
            let mut decoder = Decoder::default();
            let mut frames = Vec::new();
            decoder.push(&transcript.as_bytes()[..split], &mut frames);
            decoder.push(&transcript.as_bytes()[split..], &mut frames);

            assert_eq!(
                frames, whole,
                "splitting at byte {split} changed the frames"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_replaced_rather_than_fatal() {
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        decoder.push(b"data: \xff\xfe\n\n", &mut frames);

        assert_eq!(frames.len(), 1, "a bad byte should not lose the frame");
        assert!(frames[0].data.contains('\u{fffd}'));
    }

    #[tokio::test]
    async fn the_adapter_yields_frames_and_then_the_transport_error() {
        let chunks = stream::iter(vec![
            Ok::<&[u8], &str>(b"event: a\ndata: 1\n".as_slice()),
            Ok(b"\nevent: b\ndata: 2\n\n".as_slice()),
            Err("connection reset"),
        ]);

        let seen: Vec<Result<Frame, &str>> = frames(chunks).collect().await;

        assert_eq!(
            seen,
            vec![
                Ok(frame(Some("a"), "1")),
                Ok(frame(Some("b"), "2")),
                Err("connection reset"),
            ]
        );
    }

    #[tokio::test]
    async fn the_adapter_drops_what_a_truncated_body_left_half_written() {
        let chunks = stream::iter(vec![Ok::<&[u8], Infallible>(
            b"data: kept\n\ndata: half".as_slice(),
        )]);

        let seen: Vec<Result<Frame, Infallible>> = frames(chunks).collect().await;

        assert_eq!(seen, vec![Ok(frame(None, "kept"))]);
    }
}
