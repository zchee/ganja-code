use super::*;

#[test]
fn a_trace_records_only_event_identity_never_text_or_arguments() {
    let file = tempfile::tempfile().expect("trace file");
    let copy = file.try_clone().expect("reader handle");
    let mut log = TraceLog(BufWriter::new(file));
    let id = |seq| format!("01998ad0-0000-7000-8000-000000000000-{seq}").parse().expect("event id");
    log.frame(&id(10), "connected", None).expect("initial head");
    log.event(
        &id(11),
        &ganja_protocol::Event::PartDelta {
            session_id: "session".to_owned().into(),
            message_id: "message".to_owned().into(),
            part_id: "part".to_owned().into(),
            delta: "secret-text-canary".to_owned(),
        },
    )
    .expect("event trace");
    log.frame(&id(12), "message_finished", Some("message".to_owned())).expect("terminal");
    drop(log);
    let mut copy = copy;
    use std::io::{Read as _, Seek as _};
    copy.rewind().expect("rewind");
    let mut contents = String::new();
    copy.read_to_string(&mut contents).expect("trace contents");
    assert!(!contents.contains("secret-text-canary"));
    let trace = read(contents.as_bytes()).expect("trace decodes");
    assert_eq!(trace.initial.seq, 10);
    assert_eq!(trace.terminal.seq, 12);
    assert_eq!(trace.frames.len(), 2);
}

#[test]
fn a_declared_gap_or_missing_terminal_cannot_be_read_as_complete_evidence() {
    let initial = TraceFrame {
        epoch: "epoch".to_owned(),
        seq: 0,
        event_type: "connected".to_owned(),
        message_id: None,
    };
    let initial = serde_json::to_string(&initial).expect("initial frame");
    assert_eq!(read(initial.as_bytes()), Err(TraceError::Interval));
    let gap = TraceFrame {
        epoch: "epoch".to_owned(),
        seq: 4,
        event_type: "gap".to_owned(),
        message_id: None,
    };
    let text = format!("{initial}\n{}\n", serde_json::to_string(&gap).expect("gap frame"));
    assert_eq!(read(text.as_bytes()), Err(TraceError::Gap));
}
