use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret as _, SecretString};
use tokio_util::sync::CancellationToken;

use super::super::device::harness::{Reply, StalledClock, TestClock, serve};
use super::super::pkce::challenge_for;
use super::super::{AuthError, OauthCredential, RefreshOauth as _, now_ms, storage_key};
use super::{
    Flow, HARD_ERROR_LIMIT, LoginError, POLL_DEADLINE, PROVIDER_ID, Refresh, credential_from,
    login_flow, login_flow_at,
};

/// A canary that must never reach a message, a rendering or a log.
const REFRESH_CANARY: &str = "cursor-refresh-canary-AAAA";

/// What a finished poll delivers.
const TOKENS: &str = r#"{"accessToken":"at-cursor-1","refreshToken":"rt-cursor-1"}"#;

/// A pending answer: the recorded signal is the status, and the body —
/// which real servers fill with an error page — must be ignored.
fn pending() -> Reply {
    Reply::new(404, r#"{"error":"cursor-pending-canary"}"#)
}

/// A flow against `url`, driven by `clock`.
fn flow_at(url: &str, clock: Arc<dyn super::Clock>) -> Flow {
    login_flow_at(url).expect("a client builds").with_clock(clock)
}

/// A credential whose refresh token is the canary.
fn stored() -> OauthCredential {
    OauthCredential::new(
        SecretString::from(REFRESH_CANARY),
        SecretString::from("at-cursor-old"),
        now_ms(),
    )
}

/// The query a request's path carries, decoded by the crate that encoded
/// it.
fn query_of(path: &str) -> HashMap<String, String> {
    let (_, query) = path.split_once('?').expect("the poll carries a query");

    url::form_urlencoded::parse(query.as_bytes())
        .map(|(field, value)| (field.into_owned(), value.into_owned()))
        .collect()
}

/// A three-segment JWT claiming to expire at `exp_s`, unsigned the way
/// [`super::token_deadline_ms`] reads one.
fn jwt_expiring_at(exp_s: u64) -> SecretString {
    SecretString::from(format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp_s}}}"#)),
        URL_SAFE_NO_PAD.encode("signature")
    ))
}

#[test]
fn ganja_files_a_cursor_login_under_cursor_and_asks_cursors_own_hosts() {
    assert_eq!(PROVIDER_ID, crate::provider::cursor::ID);
    assert_eq!(
        storage_key(PROVIDER_ID),
        "cursor",
        "upstream's plugin stores under the provider's own name, so there \
             is no alias to apply"
    );

    // Not the constants asserted against themselves: what this catches is
    // the two being handed to the constructor the wrong way round.
    let flow = login_flow().expect("a client builds");
    assert_eq!(flow.login_url(), "https://cursor.com/loginDeepControl");
    assert_eq!(flow.poll_url(), "https://api2.cursor.sh/auth/poll");
}

#[test]
fn the_deep_link_carries_the_challenge_the_pairing_id_and_the_cli_return_target() {
    let flow = flow_at("http://127.0.0.1:1", TestClock::at(0));
    let login = flow.start().expect("the platform has a random source");

    assert!(login.url().starts_with("http://127.0.0.1:1/loginDeepControl?"), "{}", login.url());
    let query = query_of(login.url());
    assert_eq!(
        query.get("challenge").map(String::as_str),
        Some(challenge_for(login.pkce.verifier().expose_secret()).as_str()),
        "the published digest has to be of the verifier the poll will present"
    );
    assert_eq!(
        query.get("uuid").map(String::as_str),
        Some(login.uuid.expose_secret()),
        "the browser and the poll have to agree on the pairing id"
    );
    assert_eq!(query.get("mode").map(String::as_str), Some("login"));
    assert_eq!(query.get("redirectTarget").map(String::as_str), Some("cli"));

    // The id is the shape `crypto.randomUUID()` mints, in case the server
    // validates what pairs its browser tab with this terminal.
    let uuid: Vec<char> = login.uuid.expose_secret().chars().collect();
    assert_eq!(uuid.len(), 36, "{uuid:?}");
    for index in [8, 13, 18, 23] {
        assert_eq!(uuid[index], '-', "{uuid:?}");
    }
    assert_eq!(uuid[14], '4', "the version nibble names v4: {uuid:?}");
    assert!(matches!(uuid[19], '8' | '9' | 'a' | 'b'), "the variant bits are RFC 9562's: {uuid:?}");
    for (index, character) in uuid.iter().enumerate() {
        if ![8, 13, 18, 23].contains(&index) {
            assert!(character.is_ascii_hexdigit() && !character.is_ascii_uppercase(), "{uuid:?}");
        }
    }

    // Two logins never share a pairing id, for the reason two never share
    // a state: the id is what the tokens are collected with.
    let second = flow.start().expect("the platform has a random source");
    assert_ne!(
        login.uuid.expose_secret(),
        second.uuid.expose_secret(),
        "a shared pairing id would let one login collect another's tokens"
    );
}

#[tokio::test]
async fn a_pending_login_is_polled_on_ganjas_own_backoff_until_the_tokens_arrive() {
    let endpoint = serve(vec![pending(), pending(), Reply::ok(TOKENS)]).await;
    let clock = TestClock::at(0);
    let login =
        flow_at(&endpoint.url, clock.clone()).start().expect("the platform has a random source");
    let uuid = login.uuid.expose_secret().to_owned();
    let verifier = login.pkce.verifier().expose_secret().to_owned();

    let landed =
        login.poll(&CancellationToken::new()).await.expect("the third poll carries the tokens");

    assert_eq!(landed.access.expose_secret(), "at-cursor-1");
    assert_eq!(landed.refresh.expose_secret(), "rt-cursor-1");
    // The schedule is asserted on the waits that were asked for, never on
    // wall time: 1s, then 2s, and the delivery ends it before a third.
    assert_eq!(clock.waits(), vec![Duration::from_secs(1), Duration::from_secs(2)]);
    assert_eq!(endpoint.count(), 3);
    for index in 0..3 {
        let request = endpoint.request(index);
        assert!(request.head.starts_with("GET "), "the poll is a GET: {}", request.head);
        let query = query_of(request.path());
        assert_eq!(query.get("uuid"), Some(&uuid), "poll {index}");
        assert_eq!(query.get("verifier"), Some(&verifier), "poll {index}");
    }
}

#[tokio::test]
async fn the_backoff_doubles_to_its_cap_and_no_further() {
    let endpoint = serve((0..6).map(|_| pending()).chain([Reply::ok(TOKENS)]).collect()).await;
    let clock = TestClock::at(0);
    let login =
        flow_at(&endpoint.url, clock.clone()).start().expect("the platform has a random source");

    login.poll(&CancellationToken::new()).await.expect("the seventh poll carries the tokens");

    assert_eq!(
        clock.waits(),
        [1_000, 2_000, 4_000, 8_000, 8_000, 8_000].map(Duration::from_millis).to_vec(),
        "1s doubling to the 8s cap, then holding there"
    );
}

#[tokio::test]
async fn three_consecutive_failures_abort_where_a_pending_answer_resets_the_count() {
    let endpoint = serve(vec![
        Reply::new(500, r#"{"error":"cursor-body-canary"}"#),
        pending(),
        Reply::new(502, r#"{"error":"cursor-body-canary"}"#),
        // A success carrying no tokens is the server answering
        // unintelligibly, and counts like any other failure.
        Reply::ok(r#"{"unexpected":"shape"}"#),
        Reply::new(500, r#"{"error":"cursor-body-canary"}"#),
    ])
    .await;
    let login =
        flow_at(&endpoint.url, TestClock::at(0)).start().expect("the platform has a random source");

    let refused = login
        .poll(&CancellationToken::new())
        .await
        .expect_err("the third consecutive failure ends the login");

    let LoginError::Aborted { failures, reason } = refused else {
        panic!("the abort has to say it was one: {refused}");
    };
    assert_eq!(failures, HARD_ERROR_LIMIT);
    assert_eq!(
        reason, "HTTP 500",
        "a status and nothing else — never the body a failing endpoint echoed"
    );
    assert_eq!(
        endpoint.count(),
        5,
        "the pending answer reset the count, or this would have ended at three"
    );
}

#[tokio::test]
async fn a_login_nobody_finishes_stops_at_its_deadline() {
    // More answers than the budget can spend, so the end is the clock's.
    let endpoint = serve((0..48).map(|_| pending()).collect()).await;
    let clock = TestClock::at(0);
    let login =
        flow_at(&endpoint.url, clock.clone()).start().expect("the platform has a random source");

    let refused = login
        .poll(&CancellationToken::new())
        .await
        .expect_err("nobody ever finished in the browser");

    assert!(
        matches!(refused, LoginError::TimedOut { after } if after == POLL_DEADLINE),
        "{refused}"
    );
    let waited: u128 = clock.waits().iter().map(Duration::as_millis).sum();
    assert_eq!(
        waited,
        POLL_DEADLINE.as_millis(),
        "every wait is clamped to what is left, so the budget is spent \
             exactly and never overshot"
    );
}

#[tokio::test]
async fn a_cancelled_login_never_reaches_the_endpoint() {
    let endpoint = serve(Vec::new()).await;
    let login =
        flow_at(&endpoint.url, TestClock::at(0)).start().expect("the platform has a random source");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let refused = login.poll(&cancel).await.expect_err("a cancelled login polls nothing");

    assert!(matches!(refused, LoginError::Cancelled), "{refused}");
    assert_eq!(endpoint.count(), 0);
}

#[tokio::test]
async fn a_cancel_during_the_wait_ends_the_poll_promptly() {
    let endpoint = serve(vec![pending()]).await;
    let login = flow_at(&endpoint.url, Arc::new(StalledClock))
        .start()
        .expect("the platform has a random source");
    let cancel = CancellationToken::new();

    let polling = tokio::spawn({
        let cancel = cancel.clone();
        async move { login.poll(&cancel).await }
    });
    // The wait never ends on its own, so only the cancellation can
    // return — whether it lands before the first poll or inside the
    // stalled sleep.
    cancel.cancel();

    let refused = polling
        .await
        .expect("the poll task runs")
        .expect_err("only the cancellation could end a stalled wait");
    assert!(matches!(refused, LoginError::Cancelled), "{refused}");
}

#[test]
fn a_credential_expires_five_minutes_before_its_tokens_own_deadline_or_an_hour_from_now() {
    // Far enough out that the margin cannot land in the past.
    let exp_s: u64 = 4_000_000_000;
    let claimed = credential_from(jwt_expiring_at(exp_s), Some(SecretString::from("rt")));
    assert_eq!(
        claimed.expires,
        exp_s * 1_000 - 5 * 60 * 1_000,
        "the token's own deadline, five minutes early"
    );

    let before = now_ms();
    let opaque = credential_from(SecretString::from("at-opaque"), None);
    let after = now_ms();
    assert!(
        (before + 3_600_000..=after + 3_600_000).contains(&opaque.expires),
        "a token whose claims will not read is assumed to live an hour: \
             {} not in [{before}+1h, {after}+1h]",
        opaque.expires
    );
    assert!(
        opaque.refresh.expose_secret().is_empty(),
        "a delivery with no refresh token leaves the field blank rather \
             than borrowing the access token"
    );
}

#[tokio::test]
async fn a_renewal_rotates_the_stored_pair_with_the_bearer_the_endpoint_wants() {
    let endpoint =
        serve(vec![Reply::ok(r#"{"accessToken":"at-rotated","refreshToken":"rt-rotated"}"#)]).await;

    let renewed = Refresh::at(format!("{}/auth/exchange_user_api_key", endpoint.url))
        .expect("a client builds")
        .refresh(PROVIDER_ID, &stored())
        .await
        .expect("the endpoint answered a fresh pair");

    assert_eq!(renewed.access.expose_secret(), "at-rotated");
    assert_eq!(
        renewed.refresh.expose_secret(),
        "rt-rotated",
        "the endpoint rotated, so the new token is the stored one"
    );

    let request = endpoint.request(0);
    assert_eq!(request.path(), "/auth/exchange_user_api_key");
    assert!(request.head.starts_with("POST "), "the renewal is a POST: {}", request.head);
    assert!(
        request.has_header("authorization", &format!("Bearer {REFRESH_CANARY}")),
        "the bearer is the refresh token, which is the recorded shape"
    );
    assert!(request.has_header("content-type", "application/json"));
    assert_eq!(request.body, "{}", "the recorded body is the literal empty object");
}

#[tokio::test]
async fn a_renewal_with_no_new_refresh_token_keeps_the_old_one() {
    let endpoint = serve(vec![Reply::ok(r#"{"accessToken":"at-rotated"}"#)]).await;

    let renewed = Refresh::at(format!("{}/auth/exchange_user_api_key", endpoint.url))
        .expect("a client builds")
        .refresh(PROVIDER_ID, &stored())
        .await
        .expect("an answer with only an access token still renews");

    assert_eq!(
        renewed.refresh.expose_secret(),
        REFRESH_CANARY,
        "an endpoint that said nothing about the refresh token has not revoked it"
    );
}

#[tokio::test]
async fn a_refusal_ends_the_credential_where_an_outage_does_not() {
    for (status, dead) in [(401, true), (403, true), (500, false), (503, false)] {
        let endpoint = serve(vec![Reply::new(status, r#"{"error":"cursor-body-canary"}"#)]).await;

        let refused = Refresh::at(format!("{}/auth/exchange_user_api_key", endpoint.url))
            .expect("a client builds")
            .refresh(PROVIDER_ID, &stored())
            .await
            .expect_err("nothing was renewed");

        let (reason, expected_dead) = match &refused {
            AuthError::ReauthRequired { reason, .. } => (reason, true),
            AuthError::RefreshUnavailable { reason, .. } => (reason, false),
            other => panic!("{status} produced the wrong error: {other}"),
        };
        assert_eq!(
            expected_dead, dead,
            "{status} has to say whether logging in again would help: {refused}"
        );
        assert!(reason.contains(&format!("HTTP {status}")), "{reason}");
        assert!(
            !reason.contains("cursor-body-canary"),
            "a refusal body reached a message: {reason}"
        );
        assert!(
            !format!("{refused}").contains(REFRESH_CANARY),
            "a token reached a message: {refused}"
        );
    }
}

#[tokio::test]
async fn a_credential_with_no_refresh_token_is_dead_without_a_round_trip() {
    let endpoint = serve(Vec::new()).await;
    let blank = OauthCredential::new(SecretString::from(""), SecretString::from("at"), 0);

    let refused = Refresh::at(endpoint.url.clone())
        .expect("a client builds")
        .refresh(PROVIDER_ID, &blank)
        .await
        .expect_err("there is nothing to present");

    assert!(matches!(refused, AuthError::ReauthRequired { .. }), "{refused}");
    assert_eq!(
        endpoint.count(),
        0,
        "saying so beats a round trip that can only come back the same"
    );
}

#[test]
fn nothing_renders_the_pairing_id_or_the_verifier() {
    let login = flow_at("http://127.0.0.1:1", TestClock::at(0))
        .start()
        .expect("the platform has a random source");
    let rendered = format!("{login:?}");

    assert!(
        !rendered.contains(login.uuid.expose_secret()),
        "the pairing id reached a Debug: {rendered}"
    );
    assert!(
        !rendered.contains(login.pkce.verifier().expose_secret()),
        "a verifier reached a Debug: {rendered}"
    );
    // The challenge is published in the URL, so its presence is what
    // makes the assertions above meaningful rather than vacuous.
    assert!(
        rendered.contains(login.pkce.challenge()),
        "the challenge is public and should still be legible: {rendered}"
    );
}
