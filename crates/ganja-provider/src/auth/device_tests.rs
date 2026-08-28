use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::harness::{Endpoint, Reply, StalledClock, TestClock, serve};
use super::{
    BodyEncoding, DEFAULT_EXPIRES_MS, DEFAULT_INTERVAL_MS, DeviceError, DeviceFlow,
    GANJA_USER_AGENT, MIN_INTERVAL_MS, POLLING_SAFETY_MARGIN_MS, UPSTREAM_USER_AGENT,
    positive_seconds_ms, reportable_code,
};
use crate::auth::copilot;

/// A device-code answer with `interval` seconds between polls and a code
/// good for `expires_in` seconds.
fn authorization(interval: &str, expires_in: &str) -> String {
    format!(
        r#"{{"device_code":"dev-code","user_code":"WXYZ-1234",
                 "verification_uri":"https://example.invalid/device",
                 "interval":{interval},"expires_in":{expires_in}}}"#
    )
}

/// What a flow says it is when the test is about something else.
///
/// Neither of the two real names, deliberately: a test that happened to
/// pass because it sent the same string a production caller sends would
/// prove nothing about the parameter carrying it.
const TEST_USER_AGENT: &str = "device-flow-test/0.0.0";

/// A flow pointed at `endpoint`, driven by `clock`.
fn device_flow(
    endpoint: &Endpoint,
    clock: Arc<dyn super::Clock>,
    encoding: BodyEncoding,
) -> DeviceFlow {
    DeviceFlow::new(
        format!("{}/device", endpoint.url),
        format!("{}/token", endpoint.url),
        "test-client",
        "test-scope",
        TEST_USER_AGENT,
        encoding,
    )
    .expect("a client builds")
    .with_clock(clock)
}

#[tokio::test]
async fn a_pending_authorization_is_polled_at_the_interval_the_server_named() {
    let clock = TestClock::at(1_000);
    let endpoint = serve(vec![
        Reply::ok(authorization("7", "600")),
        // GitHub's spelling: a 200 with the error in the body. A loop that
        // took `copilot.ts:278` literally would still pass this one.
        Reply::ok(r#"{"error":"authorization_pending"}"#),
        // xAI's spelling: the RFC's 400. A loop that took
        // `copilot.ts:278` literally fails here, which is the point.
        Reply::new(400, r#"{"error":"authorization_pending"}"#),
        Reply::ok(r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

    let started = flow.start(&cancel).await.expect("the code is issued");
    assert_eq!(started.interval(), Duration::from_secs(7));
    assert_eq!(started.deadline_ms(), 1_000 + 600_000);

    let tokens = flow.poll(&started, &cancel).await.expect("the login lands");

    assert_eq!(
        clock.waits(),
        vec![
            Duration::from_millis(7_000 + POLLING_SAFETY_MARGIN_MS),
            Duration::from_millis(7_000 + POLLING_SAFETY_MARGIN_MS),
        ],
        "each pending answer should have cost one wait of the server's \
             interval plus the safety margin"
    );
    assert_eq!(endpoint.count(), 4, "one start and three polls");
    assert_eq!(tokens.expires_in, Some(3_600));
}

#[tokio::test]
async fn a_slow_down_lets_the_server_choose_the_wait_and_otherwise_compounds() {
    let clock = TestClock::at(0);
    let endpoint = serve(vec![
        Reply::ok(authorization("5", "600")),
        // No interval named: RFC 8628 §3.5's five seconds are added.
        Reply::new(400, r#"{"error":"slow_down"}"#),
        // Still nothing named: the increment compounds rather than being
        // recomputed from the original interval.
        Reply::new(400, r#"{"error":"slow_down"}"#),
        // Named, positive, numeric: the server's answer wins outright.
        Reply::new(400, r#"{"error":"slow_down","interval":11}"#),
        // The raised interval persists into the next ordinary wait.
        Reply::new(400, r#"{"error":"authorization_pending"}"#),
        Reply::ok(r#"{"access_token":"at-1"}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

    let started = flow.start(&cancel).await.expect("the code is issued");
    flow.poll(&started, &cancel).await.expect("the login lands");

    let margin = POLLING_SAFETY_MARGIN_MS;
    assert_eq!(
        clock.waits(),
        vec![
            Duration::from_millis(10_000 + margin),
            Duration::from_millis(15_000 + margin),
            Duration::from_millis(11_000 + margin),
            Duration::from_millis(11_000 + margin),
        ],
        "5s +5 = 10, +5 again = 15, then the server's 11 replaces it and stays"
    );
}

#[tokio::test]
async fn every_terminal_error_ends_the_loop_where_it_stands() {
    for (body, status, expected) in [
        (r#"{"error":"access_denied"}"#, 400, "denied"),
        (r#"{"error":"authorization_denied"}"#, 400, "denied"),
        (r#"{"error":"expired_token"}"#, 400, "expired"),
        (r#"{"error":"invalid_client"}"#, 400, "refused"),
        // No error named at all, which only a status can end.
        (r#"{}"#, 503, "status"),
    ] {
        let clock = TestClock::at(0);
        let endpoint = serve(vec![
            Reply::ok(authorization("5", "600")),
            Reply::new(status, body),
            // Deliberately available: a loop that polled again would be
            // served this and succeed, so the count is what proves it did
            // not.
            Reply::ok(r#"{"access_token":"never"}"#),
        ])
        .await;
        let cancel = CancellationToken::new();
        let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");
        let failure =
            flow.poll(&started, &cancel).await.expect_err("a terminal error is not a login");

        assert!(
            matches!(
                (&failure, expected),
                (DeviceError::Denied, "denied")
                    | (DeviceError::CodeExpired, "expired")
                    | (DeviceError::Refused { .. }, "refused")
                    | (DeviceError::Status { .. }, "status")
            ),
            "{body} with {status} should be {expected}, got {failure:?}"
        );
        assert_eq!(endpoint.count(), 2, "{body} should have ended the loop, not been polled past");
        assert!(clock.waits().is_empty(), "{body} should have cost no wait");
    }
}

#[tokio::test]
async fn a_garbage_interval_falls_back_to_the_default_instead_of_spinning() {
    for named in ["null", "0", "-5", r#""NaN""#, r#""""#, "false"] {
        let clock = TestClock::at(0);
        let endpoint = serve(vec![
            Reply::ok(authorization(named, "600")),
            Reply::new(400, r#"{"error":"authorization_pending"}"#),
            Reply::ok(r#"{"access_token":"at-1"}"#),
        ])
        .await;
        let cancel = CancellationToken::new();
        let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");
        assert_eq!(
            started.interval(),
            Duration::from_millis(DEFAULT_INTERVAL_MS),
            "an interval of {named} is not an interval"
        );

        flow.poll(&started, &cancel).await.expect("the login lands");

        assert_eq!(
            clock.waits(),
            vec![Duration::from_millis(DEFAULT_INTERVAL_MS + POLLING_SAFETY_MARGIN_MS)],
            "an interval of {named} should have waited the default, not nothing"
        );
    }
}

#[tokio::test]
async fn an_interval_under_the_floor_is_raised_to_it() {
    let clock = TestClock::at(0);
    let endpoint = serve(vec![
        Reply::ok(authorization("0.2", "600")),
        Reply::new(400, r#"{"error":"authorization_pending"}"#),
        Reply::ok(r#"{"access_token":"at-1"}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

    let started = flow.start(&cancel).await.expect("the code is issued");
    flow.poll(&started, &cancel).await.expect("the login lands");

    assert_eq!(started.interval(), Duration::from_millis(MIN_INTERVAL_MS));
    assert_eq!(
        clock.waits(),
        vec![Duration::from_millis(MIN_INTERVAL_MS + POLLING_SAFETY_MARGIN_MS)]
    );
}

#[tokio::test]
async fn a_code_that_is_never_entered_stops_at_its_deadline() {
    let clock = TestClock::at(0);
    // Four pending answers at 10s + 3s each is 52s of waiting against a
    // 30s code: the loop must stop before it runs out of answers.
    let endpoint = serve(
        std::iter::once(Reply::ok(authorization("10", "30")))
            .chain(
                std::iter::repeat_with(|| Reply::new(400, r#"{"error":"authorization_pending"}"#))
                    .take(4),
            )
            .collect(),
    )
    .await;
    let cancel = CancellationToken::new();
    let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

    let started = flow.start(&cancel).await.expect("the code is issued");
    let failure =
        flow.poll(&started, &cancel).await.expect_err("a code that expired is not a login");

    assert!(
        matches!(failure, DeviceError::DeadlineExceeded),
        "expected a deadline, got {failure:?}"
    );
    assert_eq!(
        clock.waits(),
        vec![
            Duration::from_millis(13_000),
            Duration::from_millis(13_000),
            // Clamped: 13s more would put the next poll 9s past a code
            // that has 4s left.
            Duration::from_millis(4_000),
        ],
        "the last wait must land on the deadline rather than past it"
    );
}

#[tokio::test]
async fn a_code_with_no_stated_lifetime_still_gets_a_deadline() {
    let clock = TestClock::at(500);
    let endpoint = serve(vec![Reply::ok(
        r#"{"device_code":"dev","user_code":"WXYZ",
                "verification_uri":"https://example.invalid/device","interval":5}"#,
    )])
    .await;
    let flow = device_flow(&endpoint, clock, BodyEncoding::Json);

    let started = flow.start(&CancellationToken::new()).await.expect("the code is issued");

    assert_eq!(
        started.deadline_ms(),
        500 + DEFAULT_EXPIRES_MS,
        "GitHub never sends expires_in, so the default is the only bound its flow has"
    );
}

#[tokio::test]
async fn a_cancelled_login_never_reaches_the_provider() {
    let clock = TestClock::at(0);
    let endpoint = serve(vec![Reply::ok(authorization("5", "600"))]).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let flow = device_flow(&endpoint, clock, BodyEncoding::Form);

    let failure = flow.start(&cancel).await.expect_err("a cancelled login is not a login");

    assert!(matches!(failure, DeviceError::Cancelled), "got {failure:?}");
    assert_eq!(endpoint.count(), 0, "nothing should have been asked for");
}

#[tokio::test]
async fn a_cancel_during_the_wait_ends_the_poll_promptly() {
    let endpoint = serve(vec![
        Reply::ok(authorization("5", "600")),
        Reply::new(400, r#"{"error":"authorization_pending"}"#),
        Reply::ok(r#"{"access_token":"never"}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    // A clock whose waits never end, so the only way out of the loop is
    // the cancellation itself.
    let flow = device_flow(&endpoint, Arc::new(StalledClock), BodyEncoding::Form);

    let started = flow.start(&cancel).await.expect("the code is issued");

    let waiting = tokio::spawn({
        let cancel = cancel.clone();
        async move { flow.poll(&started, &cancel).await }
    });
    while endpoint.count() < 2 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();

    let failure = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("a cancelled poll returns rather than hanging")
        .expect("the task did not panic")
        .expect_err("a cancelled login is not a login");

    assert!(matches!(failure, DeviceError::Cancelled), "got {failure:?}");
    assert_eq!(endpoint.count(), 2, "the poll should not have asked again");
}

#[tokio::test]
async fn an_endpoint_that_is_not_there_is_reported_as_unreachable() {
    let endpoint = serve(Vec::new()).await;
    let flow = device_flow(&endpoint, TestClock::at(0), BodyEncoding::Form);

    let failure =
        flow.start(&CancellationToken::new()).await.expect_err("a closed listener answers nothing");

    assert!(matches!(failure, DeviceError::Unreachable { .. }), "got {failure:?}");
}

#[tokio::test]
async fn an_authorization_missing_what_it_is_for_is_refused() {
    let endpoint = serve(vec![Reply::ok(
        r#"{"user_code":"WXYZ","verification_uri":"https://example.invalid/d"}"#,
    )])
    .await;
    let flow = device_flow(&endpoint, TestClock::at(0), BodyEncoding::Form);

    let failure = flow
        .start(&CancellationToken::new())
        .await
        .expect_err("an authorization with no device code cannot be polled");

    assert!(matches!(failure, DeviceError::Malformed { .. }), "got {failure:?}");
}

#[test]
fn a_seconds_field_is_read_only_when_it_is_a_positive_number() {
    use serde_json::{Value, json};

    for named in [json!(null), json!(0), json!(-5), json!("NaN"), json!(""), json!(true)] {
        assert_eq!(positive_seconds_ms(Some(&named)), None, "{named} is not a number of seconds");
    }
    assert_eq!(positive_seconds_ms(None), None);
    assert_eq!(positive_seconds_ms(Some(&json!(7))), Some(7_000));
    // Coerced, because upstream's `Number("5")` is.
    assert_eq!(positive_seconds_ms(Some(&json!("5"))), Some(5_000));
    assert_eq!(positive_seconds_ms(Some(&json!(0.25))), Some(250));
    // Absurd rather than wrapped: the deadline is what bounds it.
    assert_eq!(positive_seconds_ms(Some(&Value::from(1e30))), Some(u64::MAX));
}

#[test]
fn an_error_code_that_is_not_one_is_not_repeated() {
    assert_eq!(reportable_code("invalid_grant"), "invalid_grant");
    assert_eq!(reportable_code("slow-down.2"), "slow-down.2");
    // The shape a token, a stack trace or a quoted request has.
    for unsafe_code in [
        "",
        "gho_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "invalid grant",
        "bearer=sk-1234",
    ] {
        assert_eq!(
            reportable_code(unsafe_code),
            super::UNREPEATABLE,
            "{unsafe_code:?} should not be repeated back"
        );
    }
}

#[tokio::test]
async fn each_flow_sends_the_user_agent_its_own_caller_named() {
    const ONE: &str = "one-name/1.1.1";
    const ANOTHER: &str = "another-name/2.2.2";

    // Both directions, because the failure this guards against is a shared
    // value reappearing under two names — which a single construction
    // cannot tell apart from a parameter that works.
    for (mine, somebody_elses) in [(ONE, ANOTHER), (ANOTHER, ONE)] {
        let endpoint = serve(vec![Reply::ok(authorization("5", "600"))]).await;
        let flow = DeviceFlow::new(
            format!("{}/device", endpoint.url),
            format!("{}/token", endpoint.url),
            "test-client",
            "test-scope",
            mine,
            BodyEncoding::Form,
        )
        .expect("a client builds");

        assert_eq!(flow.user_agent(), mine);

        flow.start(&CancellationToken::new()).await.expect("the code is issued");

        let request = endpoint.request(0);
        assert!(
            request.has_header("user-agent", mine),
            "the flow should send what this caller supplied: {}",
            request.head
        );
        assert!(
            !request.has_header("user-agent", somebody_elses),
            "and never the other caller's, which is the whole reason this \
                 is a field rather than a constant: {}",
            request.head
        );
    }
}

#[test]
fn the_borrowed_identity_and_ganjas_own_never_name_the_same_thing() {
    // The boundary itself, asserted rather than described. Collapsing the
    // two constants into one is the refactor that would send
    // `ganja-code/…` to `api.githubcopilot.com`, whose named trigger for
    // suspending an account is mismatched client telemetry.
    assert_ne!(
        UPSTREAM_USER_AGENT, GANJA_USER_AGENT,
        "these two exist to be different; one of them is a name this \
             build is not entitled to use everywhere"
    );
    assert_eq!(
        UPSTREAM_USER_AGENT, "opencode/1.18.22",
        "the borrowed identity is upstream's product at the pinned version"
    );
    assert!(
        GANJA_USER_AGENT.starts_with("ganja-code/"),
        "ganja's own name is the project's, not the binary's — which is \
             also what rules out the pre-split spelling, since no string can \
             begin with both: {GANJA_USER_AGENT}"
    );

    // Every host that says what this build is. Each reaches
    // `GANJA_USER_AGENT` through a constant named for itself, so what is
    // asserted here is that they still arrive at the same answer and that
    // the answer is not the borrowed one — a per-host constant quietly
    // repointed at `UPSTREAM_USER_AGENT` would otherwise read as an
    // ordinary alias. With W4 landed this list is every host but one, and
    // the one is below.
    for (host, sent) in [
        ("auth.openai.com", crate::auth::openai::ISSUER_USER_AGENT),
        ("chatgpt.com/backend-api/codex", crate::provider::responses::CODEX_USER_AGENT),
        ("x.ai", crate::auth::grok::XAI_USER_AGENT),
    ] {
        assert_eq!(
            sent, GANJA_USER_AGENT,
            "{host} says what this build is, through its own constant"
        );
        assert_ne!(
            sent, UPSTREAM_USER_AGENT,
            "{host} was moved off the borrowed identity against a live \
                 probe; nothing may return it without one"
        );
    }

    // Copilot's device half. The chat half is pinned where it is sent,
    // beside the other three headers that were measured with it
    // (`provider::copilot`'s own header test), as a literal as well as
    // through this constant — between them, neither renaming this nor
    // repointing that one can move `api.githubcopilot.com` quietly.
    let copilot = copilot::device_flow_at(
        "https://github.invalid/login/device/code",
        "https://github.invalid/login/oauth/access_token",
    )
    .expect("a client builds");

    assert_eq!(
        copilot.user_agent(),
        UPSTREAM_USER_AGENT,
        "GitHub's device endpoints keep the borrowed identity"
    );
    assert_eq!(copilot.user_agent(), "opencode/1.18.22");
    assert_ne!(
        copilot.user_agent(),
        GANJA_USER_AGENT,
        "moving this host is a decision on its own evidence, not a \
             consequence of tidying two constants into one"
    );
}
