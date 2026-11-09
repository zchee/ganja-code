use super::*;

fn event() -> Event {
    Event::PermissionReplied {
        session_id: ganja_protocol::SessionId::ascending(),
        id: ganja_protocol::PermissionId::ascending(),
        reply: ganja_protocol::PermissionReply::Once,
    }
}

#[test]
fn replay_is_strictly_after_the_cursor_and_bounds_declare_a_gap() {
    let journal = EventJournal::with_limits(
        NonZeroUsize::new(2).expect("nonzero"),
        NonZeroUsize::new(4096).expect("nonzero"),
    );
    let initial = journal.head();
    journal.append(event());
    let first = journal.head();
    journal.append(event());
    let replay = journal.register(Some(&first));
    assert_eq!(replay.replay, "from");
    assert_eq!(journal.after(replay.seq).expect("retained").len(), 1);
    journal.append(event());
    assert_eq!(journal.register(Some(&initial)).replay, "gap");
    assert_eq!(journal.register(Some(&first)).replay, "from");
    assert_eq!(journal.register(None).replay, "fresh");
}

#[test]
fn an_oversize_event_advances_the_head_but_cannot_silently_replay() {
    let journal = EventJournal::with_limits(
        NonZeroUsize::new(2).expect("nonzero"),
        NonZeroUsize::new(1).expect("nonzero"),
    );
    let before = journal.head();
    journal.append(event());
    assert_ne!(journal.head(), before);
    assert_eq!(journal.register(Some(&before)).replay, "gap");
    assert!(journal.after(0).is_none());
}

#[test]
fn an_epoch_mismatch_and_future_sequence_declare_gaps() {
    let journal = EventJournal::new();
    assert_eq!(journal.register(Some(&EventJournal::new().head())).replay, "gap");
    let future = journal.head().trim_end_matches('0').to_owned() + "1";
    assert_eq!(journal.register(Some(&future)).replay, "gap");
}

#[test]
fn malformed_sequences_cannot_extend_or_replace_the_uuid_prefix() {
    let journal = EventJournal::new();
    let epoch = journal.epoch();
    for suffix in ["", "-", "-0-0", "--0", "-+0", "-no-sequence", "-18446744073709551616", "-０"] {
        let cursor = format!("{epoch}{suffix}");
        assert_eq!(journal.register(Some(&cursor)).replay, "gap", "malformed cursor {cursor:?}");
    }
    for cursor in ["short-0", "界".repeat(13).as_str()] {
        assert_eq!(journal.register(Some(cursor)).replay, "gap", "invalid prefix {cursor:?}");
    }
    assert_eq!(journal.register(Some(&format!("{epoch}-0"))).replay, "from");
}

#[test]
fn permission_projection_is_complete_before_the_head_is_observable() {
    let journal = EventJournal::new();
    let id = ganja_protocol::PermissionId::ascending();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    *journal.0.before_advance.lock().expect("barrier lock") = Some(barrier.clone());
    let writer = journal.clone();
    let requested = id.clone();
    let writer = std::thread::spawn(move || {
        writer.append(Event::PermissionRequested {
            session_id: ganja_protocol::SessionId::ascending(),
            id: requested,
            call_id: "call".into(),
            tool: "read".into(),
            title: "read a file".into(),
            args: serde_json::json!({}),
            directories: Vec::new(),
        })
    });
    barrier.wait();
    assert_eq!(journal.pending().list()[0].id, id, "projection precedes the head");
    assert!(journal.0.state.try_lock().is_err(), "registration cannot pass the projection barrier");
    let registration = journal.clone();
    let registration = std::thread::spawn(move || registration.register(None));
    barrier.wait();
    writer.join().expect("writer");
    assert!(registration.join().expect("registration").head.ends_with("-1"));
    *journal.0.before_advance.lock().expect("barrier lock") = None;
    assert!(journal.head().ends_with("-1"));
    assert_eq!(journal.pending().list()[0].id, id);
    journal.append(Event::PermissionReplied {
        session_id: ganja_protocol::SessionId::ascending(),
        id,
        reply: ganja_protocol::PermissionReply::Once,
    });
    assert!(journal.head().ends_with("-2"));
    assert!(journal.pending().list().is_empty());
}
