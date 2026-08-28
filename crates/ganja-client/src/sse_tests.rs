use super::{EvictedNotice, FRAMES, Frame, Frames};

/// Serve writes `event: <name>\ndata: <json>\n\n`; a splitter that only
/// worked when a whole frame arrived in one read would work in a test and
/// fail against a real socket.
#[test]
fn a_frame_split_across_reads_is_still_one_frame() {
    let mut frames = Frames::new();
    frames.push(b"event: connect");
    assert!(frames.pop().is_none());
    frames.push(b"ed\ndata: {}\n\nevent: heartbeat\ndata: {}\n\n");

    assert_eq!(frames.pop().transpose().expect("a declared frame"), Some(Frame::Connected));
    assert_eq!(frames.pop().transpose().expect("a declared frame"), Some(Frame::Heartbeat));
    assert!(frames.pop().is_none());
}

#[test]
fn a_frame_named_outside_the_vocabulary_is_a_version_mismatch() {
    let mut frames = Frames::new();
    frames.push(b"event: server.hello\ndata: {}\n\n");

    let error =
        frames.pop().expect("a complete frame").expect_err("a name outside the set is refused");
    let said = error.to_string();
    assert!(said.contains("server.hello"), "{said}");
    assert!(said.contains("different versions of ganja"), "the refusal names the mismatch: {said}");
}

#[test]
fn an_evicted_notice_round_trips_through_the_declared_shape() {
    let notice = EvictedNotice {
        kind: super::EVICTED.to_owned(),
        message: "this subscriber fell behind".to_owned(),
    };
    let written = serde_json::to_string(&notice).expect("the notice serializes");

    assert_eq!(serde_json::from_str::<EvictedNotice>(&written).expect("and reads back"), notice);
    // A field nobody declared is a server this build does not understand.
    assert!(
        serde_json::from_str::<EvictedNotice>(r#"{"type":"evicted","message":"x","why":1}"#)
            .is_err()
    );
}

#[test]
fn the_vocabulary_is_four_distinct_names() {
    let mut sorted = FRAMES;
    sorted.sort_unstable();
    sorted.windows(2).for_each(|pair| assert_ne!(pair[0], pair[1], "two frames share a name"));
    assert_eq!(FRAMES.len(), 4);
}
