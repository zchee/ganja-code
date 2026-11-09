use super::*;

#[test]
fn the_two_doors_open_independently_and_close_together() {
    let gate = WorkerGate::new();
    assert!(gate.admit(Door::Stream).is_none());
    assert!(gate.admit(Door::Rest).is_none());
    gate.open_stream();
    let stream = gate.admit(Door::Stream).expect("stream door opened");
    assert!(gate.admit(Door::Rest).is_none());
    gate.open_rest();
    let rest = gate.admit(Door::Rest).expect("REST door opened");
    gate.close();
    assert!(gate.admit(Door::Stream).is_none());
    assert!(gate.admit(Door::Rest).is_none());
    stream.finish(AttemptOutcome::Serve);
    rest.finish(AttemptOutcome::Serve);
    assert_eq!(gate.state(), GateState::default());
}

#[tokio::test]
async fn an_outstanding_request_defers_suspend_without_latching_uncertainty() {
    let gate = WorkerGate::new();
    gate.open_rest();
    let request = gate.admit(Door::Rest).expect("open");
    gate.close();
    assert_eq!(gate.drain_rest(Duration::from_millis(1)).await, Drain::Deferred);
    assert!(!gate.state().uncertain);
    request.finish(AttemptOutcome::Serve);
    assert_eq!(gate.drain_rest(Duration::from_secs(1)).await, Drain::Complete);
    assert!(!gate.state().uncertain);
}

#[tokio::test]
async fn drain_waits_for_the_actual_response() {
    let gate = WorkerGate::new();
    gate.open_rest();
    let request = gate.admit(Door::Rest).expect("open");
    gate.close();
    let waiter = tokio::spawn({
        let gate = Arc::clone(&gate);
        async move { gate.drain_rest(Duration::from_secs(1)).await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    request.finish(AttemptOutcome::Serve);
    assert_eq!(waiter.await.expect("drain task"), Drain::Complete);
}

#[test]
fn later_success_and_reopening_never_clear_the_uncertain_latch() {
    for outcome in [None, Some(AttemptOutcome::Uncertain)] {
        let gate = WorkerGate::new();
        gate.open_stream();
        let attempt = gate.admit(Door::Stream).expect("open");
        if let Some(outcome) = outcome {
            attempt.finish(outcome);
        } else {
            drop(attempt);
        }
        assert!(gate.state().uncertain);
        gate.close();
        gate.open_stream();
        gate.admit(Door::Stream).expect("reopened").finish(AttemptOutcome::Serve);
        assert!(gate.state().uncertain);
    }
}

#[test]
fn closing_an_established_stream_is_not_an_uncertain_wake() {
    let gate = WorkerGate::new();
    gate.open_stream();
    let mut stream = gate.admit(Door::Stream).expect("open");
    stream.classify(AttemptOutcome::Serve);
    gate.close();
    drop(stream);
    assert_eq!(gate.state().streams, 0);
    assert!(!gate.state().uncertain);
}
