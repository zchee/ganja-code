//! Offline delivery evidence is checked independently of the transport's own counters.

use ganja_teammate_cloud::trace::{DeliveredTrace, TraceError, TraceFrame, check};

fn frame(seq: u64, event_type: &str) -> TraceFrame {
    TraceFrame {
        epoch: "worker-incarnation".to_owned(),
        seq,
        event_type: event_type.to_owned(),
        message_id: Some("assistant-message".to_owned()),
    }
}

fn fixture() -> (DeliveredTrace, Vec<TraceFrame>, Vec<String>) {
    let manifest: Vec<_> = ["message_started", "part_started", "part_delta", "message_finished"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let frames: Vec<_> =
        manifest.iter().enumerate().map(|(i, kind)| frame(i as u64 + 11, kind)).collect();
    let trace = DeliveredTrace {
        initial: frame(10, "connected"),
        terminal: frames.last().expect("script has a terminal").clone(),
        frames: frames.clone(),
    };
    let mut journal = vec![frame(9, "inspection"), frame(10, "inspection")];
    journal.extend(frames);
    journal.push(frame(15, "inspection"));
    (trace, journal, manifest)
}

#[test]
fn inspection_frames_outside_the_interval_are_not_acceptance_evidence() {
    let (trace, journal, manifest) = fixture();
    assert_eq!(check(&trace, &journal, &manifest), Ok(()));
}

#[test]
fn a_missing_or_duplicated_delivery_fails_even_with_a_retained_terminal() {
    let (trace, journal, manifest) = fixture();
    let mut missing = trace.clone();
    missing.frames.remove(1);
    assert_eq!(check(&missing, &journal, &manifest), Err(TraceError::Delivery));
    let mut duplicate = trace;
    duplicate.frames.insert(1, duplicate.frames[0].clone());
    assert_eq!(check(&duplicate, &journal, &manifest), Err(TraceError::Delivery));
}

#[test]
fn an_evicted_beginning_fails_instead_of_accepting_a_partial_interval() {
    let (trace, journal, manifest) = fixture();
    assert_eq!(check(&trace, &journal[3..], &manifest), Err(TraceError::Retention));
}

#[test]
fn a_different_script_or_epoch_is_not_equivalent_evidence() {
    let (mut trace, journal, mut manifest) = fixture();
    manifest[2] = "part_updated".to_owned();
    assert_eq!(check(&trace, &journal, &manifest), Err(TraceError::Manifest));
    trace.terminal.epoch = "new-incarnation".to_owned();
    assert_eq!(check(&trace, &journal, &manifest), Err(TraceError::Epoch));
}
