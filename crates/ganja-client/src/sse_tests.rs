use super::{EvictedNotice, FRAMES, Frame, Frames};

/// Serve writes `event: <name>\ndata: <json>\n\n`; a splitter that only
/// worked when a whole frame arrived in one read would work in a test and
/// fail against a real socket.
#[test]
fn a_frame_split_across_reads_is_still_one_frame() {
    let mut frames = Frames::new();
    frames.push(b"id: 01998ad0-0000-7000-8000-00000000d505-0\nevent: connect");
    assert!(frames.pop().is_none());
    frames.push(b"ed\ndata: {\"head\":\"01998ad0-0000-7000-8000-00000000d505-0\",\"replay\":\"fresh\"}\n\nid: 01998ad0-0000-7000-8000-00000000d505-0\nevent: heartbeat\ndata: {}\n\n");

    let id: super::EventId = "01998ad0-0000-7000-8000-00000000d505-0".parse().expect("valid id");
    assert_eq!(
        frames.pop().transpose().expect("a declared frame"),
        Some(Frame::Connected(
            id.clone(),
            super::Connected { head: id.clone(), replay: super::Replay::Fresh }
        ))
    );
    assert_eq!(frames.pop().transpose().expect("a declared frame"), Some(Frame::Heartbeat(id)));
    assert!(frames.pop().is_none());
}

#[test]
fn a_frame_named_outside_the_vocabulary_is_a_version_mismatch() {
    let mut frames = Frames::new();
    frames.push(b"id: 01998ad0-0000-7000-8000-00000000d505-0\nevent: server.hello\ndata: {}\n\n");

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
fn event_ids_are_canonical_and_bounded() {
    let epoch = "01998ad0-0000-7000-8000-00000000d505";
    for seq in [0, 1, u64::MAX] {
        let text = format!("{epoch}-{seq}");
        let id: super::EventId = text.parse().expect("valid cursor");
        assert_eq!(id.epoch(), epoch);
        assert_eq!(id.seq(), seq);
        assert_eq!(id.to_string(), text);
    }
    for text in [
        "",
        "epoch-1",
        "-1",
        "01998ad0-0000-4000-8000-00000000d505-1",
        "01998ad0-0000-7000-7000-00000000d505-1",
        "01998ad0-0000-7000-8000-00000000d505--1",
        "01998ad0-0000-7000-8000-00000000d505-01",
        "01998ad0-0000-7000-8000-00000000d505-18446744073709551616",
    ] {
        assert!(
            matches!(text.parse::<super::EventId>(), Err(crate::ClientError::Skew { .. })),
            "{text:?}"
        );
    }
}

#[test]
fn the_vocabulary_is_four_distinct_names() {
    let mut sorted = FRAMES;
    sorted.sort_unstable();
    sorted.windows(2).for_each(|pair| assert_ne!(pair[0], pair[1], "two frames share a name"));
    assert_eq!(FRAMES.len(), 4);
}
