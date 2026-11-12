use super::*;

#[test]
fn structured_http_and_decode_errors_have_fixed_kinds() {
    let refused = |status| ClientError::Refused {
        method: "GET",
        path: "/global/health".to_owned(),
        status,
        from_serve: false,
        body: "response-body-canary".to_owned(),
    };
    for (error, expected) in [
        (refused(404), Kind::Http4xx),
        (refused(502), Kind::Http5xx),
        (refused(302), Kind::Http3xx),
        (refused(700), Kind::HttpOther),
        (
            ClientError::Unauthorized { address: "address-canary".to_owned(), from_serve: false },
            Kind::Http4xx,
        ),
        (ClientError::Skew { detail: "body-canary".to_owned() }, Kind::Decode),
        (
            ClientError::Oversized { method: "GET", path: "/global/health".to_owned(), cap: 1 },
            Kind::Other,
        ),
    ] {
        assert_eq!(classify(&error), expected);
    }
}

#[test]
fn io_kinds_are_found_through_an_error_source_chain() {
    #[derive(Debug, thiserror::Error)]
    #[error("outer transport")]
    struct Outer(#[source] std::io::Error);
    for (kind, expected) in [
        (std::io::ErrorKind::ConnectionRefused, Kind::ConnectionRefused),
        (std::io::ErrorKind::ConnectionReset, Kind::ConnectionReset),
        (std::io::ErrorKind::TimedOut, Kind::TimedOut),
        (std::io::ErrorKind::BrokenPipe, Kind::Other),
    ] {
        assert_eq!(transport_kind(&Outer(std::io::Error::from(kind))), expected);
    }
}

#[test]
fn completed_failures_and_a_success_have_consistent_totals() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut tally = HealthWait::new("retry-worker", &cancel);
    for _ in 0..2 {
        tally.begin();
        tally.failed(&ClientError::Refused {
            method: "GET",
            path: "/global/health".to_owned(),
            status: 403,
            from_serve: false,
            body: "not retained".to_owned(),
        });
    }
    tally.begin();
    tally.ready();
    assert_eq!(tally.attempts, 3);
    assert_eq!(tally.failures, 2);
    assert_eq!(tally.kinds[Kind::Http4xx as usize], 2);
    assert_eq!(tally.kinds.iter().sum::<u64>(), 2);
    assert_eq!(tally.outcome, "ready");
}

#[test]
fn a_deadline_counts_only_an_in_flight_probe_as_an_additional_timeout() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut sleeping = HealthWait::new("sleeping-worker", &cancel);
    sleeping.begin();
    sleeping.failed(&ClientError::Skew { detail: "not retained".to_owned() });
    sleeping.deadline();
    assert_eq!(sleeping.attempts, 1);
    assert_eq!(sleeping.failures, 1);
    assert_eq!(sleeping.kinds[Kind::TimedOut as usize], 0);
    let mut pending = HealthWait::new("pending-worker", &cancel);
    pending.begin();
    pending.deadline();
    assert_eq!(pending.attempts, 1);
    assert_eq!(pending.failures, 1);
    assert_eq!(pending.kinds[Kind::TimedOut as usize], 1);
    assert_eq!(pending.outcome, "deadline_reached");
}

#[test]
fn owner_cancellation_does_not_invent_a_timed_out_probe() {
    let capture = ganja_testkit::LogCapture::default();
    let subscriber =
        tracing_subscriber::fmt().with_writer(capture.clone()).with_ansi(false).finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut tally = HealthWait::new("cancelled-worker", &cancel);
    tally.begin();
    cancel.cancel();
    drop(tally);
    let logged = capture.logged();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    assert!(logged.contains("outcome=\"cancelled\""), "{logged}");
    assert!(logged.contains("attempts=1 failures=0"), "{logged}");
    assert!(logged.contains("timed_out=0"), "{logged}");
}
