use super::{
    END_STREAM_FLAG, Frame, ProviderError, Splitter, end_stream_error, envelope, http_status,
};

/// The exact EndStream JSON the live probe recorded, 104 bytes, which is
/// the `len 0x68` its frame prefix declared.
const RECORDED_END_STREAM: &str = "{\"error\":{\"code\":\"invalid_argument\",\
        \"message\":\"First message must be a run request or prewarm request\"}}";

/// The two-frame body the live probe recorded on the Run RPC: a data
/// frame holding a heartbeat update, then the EndStream frame above.
fn recorded_body() -> Vec<u8> {
    assert_eq!(RECORDED_END_STREAM.len(), 0x68, "the recorded length");

    let mut body = vec![0x00, 0x00, 0x00, 0x00, 0x04, 0x0a, 0x02, 0x6a, 0x00];
    body.extend_from_slice(&[END_STREAM_FLAG, 0x00, 0x00, 0x00, 0x68]);
    body.extend_from_slice(RECORDED_END_STREAM.as_bytes());

    body
}

/// Every frame `splitter` has completed so far.
fn drained(splitter: &mut Splitter) -> Vec<Frame> {
    let mut frames = Vec::new();
    while let Some(frame) = splitter.frame().expect("the fixture bodies parse") {
        frames.push(frame);
    }

    frames
}

#[test]
fn the_recorded_run_body_splits_into_its_two_frames() {
    let mut splitter = Splitter::default();
    splitter.push(&recorded_body());
    let split = drained(&mut splitter);

    assert_eq!(split.len(), 2);
    assert!(!split[0].is_end_stream());
    assert_eq!(split[0].payload, [0x0a, 0x02, 0x6a, 0x00]);
    assert!(split[1].is_end_stream());

    let (code, message) = end_stream_error(&split[1].payload)
        .expect("the recorded JSON parses")
        .expect("the recorded frame carried an error");
    assert_eq!(code, "invalid_argument");
    assert!(message.contains("must be a run request"), "{message}");
}

/// The transport owes the splitter nothing about boundaries: fed one
/// byte at a time — every frame split across every possible seam — the
/// same two frames come out.
#[test]
fn a_frame_split_wherever_the_socket_likes_is_reassembled() {
    let mut splitter = Splitter::default();
    let mut frames = Vec::new();

    for byte in recorded_body() {
        splitter.push(&[byte]);
        frames.extend(drained(&mut splitter));
    }

    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].payload, [0x0a, 0x02, 0x6a, 0x00]);
    assert!(frames[1].is_end_stream());
}

#[test]
fn the_envelope_this_build_sends_is_the_one_the_probe_sent() {
    // The probe's opening message: flag 0, length 0 — five zero bytes.
    assert_eq!(envelope(&[]), [0x00; 5]);

    let framed = envelope(&[0x0a, 0x01, 0x78]);
    assert_eq!(framed, [0x00, 0x00, 0x00, 0x00, 0x03, 0x0a, 0x01, 0x78]);
}

/// A frame the body has not finished is held, never handed out half
/// read; whether the missing rest is a truncation is the mapping's call,
/// made at end of body with the exchange's state in hand.
#[test]
fn a_body_that_ends_mid_frame_hands_nothing_back() {
    let mut body = recorded_body();
    body.truncate(body.len() - 10);

    let mut splitter = Splitter::default();
    splitter.push(&body);
    assert_eq!(
        drained(&mut splitter).len(),
        1,
        "the whole first frame arrived, and only it comes out"
    );

    // Three bytes cannot even hold a prefix.
    let mut short = Splitter::default();
    short.push(&[0x00, 0x00, 0x00]);
    assert!(short.frame().expect("nothing to refuse yet").is_none());
}

#[test]
fn bytes_after_the_end_stream_frame_are_refused() {
    let mut body = recorded_body();
    body.push(0x00);

    let mut splitter = Splitter::default();
    splitter.push(&body);
    assert!(
        splitter.frame().expect("the data frame parses").is_some(),
        "the real frames still come out"
    );
    assert!(splitter.frame().expect("the EndStream frame parses").is_some());

    let refused = splitter.frame().expect_err("nothing follows the stream's ending");
    assert!(refused.to_string().contains("EndStream"), "{refused}");
}

#[test]
fn an_end_stream_without_an_error_member_is_a_clean_end() {
    assert_eq!(end_stream_error(b"{}").expect("an empty object parses"), None);
    // Members this build does not model are not a reason to fail a turn
    // that succeeded.
    assert_eq!(end_stream_error(br#"{"metadata":{"x":"y"}}"#).expect("parses"), None);
    assert!(matches!(end_stream_error(b"not json"), Err(ProviderError::Parse(_))));
}

#[test]
fn the_status_mapping_is_the_connect_protocols_own() {
    assert_eq!(http_status("invalid_argument"), 400);
    assert_eq!(http_status("unauthenticated"), 401);
    assert_eq!(http_status("resource_exhausted"), 429);
    assert_eq!(http_status("unavailable"), 503);
    assert_eq!(http_status("internal"), 500);
    assert_eq!(http_status("a-code-from-the-future"), 500);
}
