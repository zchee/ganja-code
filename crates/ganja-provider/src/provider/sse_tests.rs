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
    Frame { event: event.map(str::to_owned), data: data.to_owned() }
}

#[test]
fn the_shapes_a_provider_can_send_all_decode() {
    let cases: Vec<(&str, &str, Vec<Frame>)> = vec![
        ("empty input", "", vec![]),
        ("a named frame", "event: ping\ndata: {}\n\n", vec![frame(Some("ping"), "{}")]),
        (
            "an unnamed frame, which is all OpenAI sends",
            "data: {\"a\":1}\n\n",
            vec![frame(None, "{\"a\":1}")],
        ),
        ("crlf terminators", "event: ping\r\ndata: {}\r\n\r\n", vec![frame(Some("ping"), "{}")]),
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
        ("a field with no colon has an empty value", "data\n\n", vec![frame(None, "")]),
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
        assert_eq!(decode_byte_by_byte(input), expected, "{name}: byte-by-byte decode");
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

        assert_eq!(frames, whole, "splitting at byte {split} changed the frames");
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
        vec![Ok(frame(Some("a"), "1")), Ok(frame(Some("b"), "2")), Err("connection reset"),]
    );
}

#[tokio::test]
async fn the_adapter_drops_what_a_truncated_body_left_half_written() {
    let chunks =
        stream::iter(vec![Ok::<&[u8], Infallible>(b"data: kept\n\ndata: half".as_slice())]);

    let seen: Vec<Result<Frame, Infallible>> = frames(chunks).collect().await;

    assert_eq!(seen, vec![Ok(frame(None, "kept"))]);
}
