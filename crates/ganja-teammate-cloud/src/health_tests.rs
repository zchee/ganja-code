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
fn a_nested_io_reset_is_not_hidden_by_an_outer_io_error() {
    let error = std::io::Error::other(std::io::Error::from(ErrorKind::ConnectionReset));
    assert_eq!(transport_kind(&error), Kind::ConnectionReset);
}

#[test]
fn a_nested_io_refusal_is_not_hidden_by_an_outer_io_error() {
    let error = std::io::Error::other(std::io::Error::from(ErrorKind::ConnectionRefused));
    assert_eq!(transport_kind(&error), Kind::ConnectionRefused);
}

#[test]
fn a_nested_io_timeout_is_not_hidden_by_an_outer_io_error() {
    let error = std::io::Error::other(std::io::Error::from(ErrorKind::TimedOut));
    assert_eq!(transport_kind(&error), Kind::TimedOut);
}

#[test]
fn transport_classification_prefers_reset_then_refusal_then_timeout() {
    for (outer, inner, expected) in [
        (ErrorKind::TimedOut, ErrorKind::ConnectionReset, Kind::ConnectionReset),
        (ErrorKind::ConnectionRefused, ErrorKind::ConnectionReset, Kind::ConnectionReset),
        (ErrorKind::TimedOut, ErrorKind::ConnectionRefused, Kind::ConnectionRefused),
        (ErrorKind::ConnectionRefused, ErrorKind::TimedOut, Kind::ConnectionRefused),
        (ErrorKind::ConnectionReset, ErrorKind::ConnectionRefused, Kind::ConnectionReset),
    ] {
        let error = std::io::Error::new(outer, std::io::Error::from(inner));
        assert_eq!(transport_kind(&error), expected, "outer={outer:?}, inner={inner:?}");
    }
}

#[test]
fn completed_failures_and_a_success_have_consistent_totals() {
    let mut tally = HealthWait::new("retry-worker");
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
    let mut sleeping = HealthWait::new("sleeping-worker");
    sleeping.begin();
    sleeping.failed(&ClientError::Skew { detail: "not retained".to_owned() });
    sleeping.deadline();
    assert_eq!(sleeping.attempts, 1);
    assert_eq!(sleeping.failures, 1);
    assert_eq!(sleeping.kinds[Kind::TimedOut as usize], 0);
    let mut pending = HealthWait::new("pending-worker");
    pending.begin();
    pending.deadline();
    assert_eq!(pending.attempts, 1);
    assert_eq!(pending.failures, 1);
    assert_eq!(pending.kinds[Kind::TimedOut as usize], 1);
    assert_eq!(pending.outcome, "deadline_reached");
}

#[test]
fn dropping_a_pending_health_wait_does_not_invent_a_timeout() {
    let capture = ganja_testkit::LogCapture::default();
    let subscriber =
        tracing_subscriber::fmt().with_writer(capture.clone()).with_ansi(false).finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let mut tally = HealthWait::new("cancelled-worker");
    tally.begin();
    drop(tally);
    let logged = capture.logged();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    assert!(logged.contains("outcome=\"cancelled\""), "{logged}");
    assert!(logged.contains("attempts=1 failures=0"), "{logged}");
    assert!(logged.contains("timed_out=0"), "{logged}");
}

#[tokio::test]
async fn aborting_a_pending_health_task_leaves_the_probe_unfinished() {
    let capture = ganja_testkit::LogCapture::default();
    let subscriber =
        tracing_subscriber::fmt().with_writer(capture.clone()).with_ansi(false).finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut tally = HealthWait::new("aborted-worker");
        tally.begin();
        started.send(()).expect("health task started");
        std::future::pending::<()>().await;
    });
    ready.await.expect("pending probe observed");
    task.abort();
    assert!(task.await.expect_err("task aborted").is_cancelled());
    let logged = capture.logged();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    assert!(logged.contains("outcome=\"cancelled\""), "{logged}");
    assert!(logged.contains("attempts=1 failures=0 unfinished=1"), "{logged}");
    assert!(logged.contains("timed_out=0"), "{logged}");
}

#[test]
fn unwinding_a_health_wait_leaves_the_probe_unfinished() {
    let capture = ganja_testkit::LogCapture::default();
    let subscriber =
        tracing_subscriber::fmt().with_writer(capture.clone()).with_ansi(false).finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let result = std::panic::catch_unwind(|| {
        let mut tally = HealthWait::new("unwound-worker");
        tally.begin();
        panic!("unwind the pending health wait");
    });
    assert!(result.is_err());
    let logged = capture.logged();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    assert!(logged.contains("outcome=\"cancelled\""), "{logged}");
    assert!(logged.contains("attempts=1 failures=0 unfinished=1"), "{logged}");
    assert!(logged.contains("timed_out=0"), "{logged}");
}

#[tokio::test(start_paused = true)]
async fn simultaneous_deadline_and_cancellation_keep_the_selected_outcome() {
    for deadline_first in [true, false] {
        let capture = ganja_testkit::LogCapture::default();
        let subscriber =
            tracing_subscriber::fmt().with_writer(capture.clone()).with_ansi(false).finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut tally = HealthWait::new("racing-worker");
        tally.begin();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(1));
        tokio::pin!(deadline);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        cancel.cancel();
        if deadline_first {
            tokio::select! {
                biased;
                () = &mut deadline => tally.deadline(),
                () = cancel.cancelled() => panic!("deadline must be observed first"),
            }
        } else {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                () = &mut deadline => panic!("cancellation must be observed first"),
            }
        }
        drop(tally);
        let logged = capture.logged();
        assert_eq!(logged.lines().count(), 1, "{logged}");
        let expected = if deadline_first { "deadline_reached" } else { "cancelled" };
        assert!(logged.contains(&format!("outcome=\"{expected}\"")), "{logged}");
        assert!(
            logged.contains(if deadline_first {
                "attempts=1 failures=1 unfinished=0"
            } else {
                "attempts=1 failures=0 unfinished=1"
            }),
            "{logged}"
        );
    }
}
