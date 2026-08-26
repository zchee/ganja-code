use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;

use super::{
    super::{
        AuthError, AuthErrorKind, OauthCredential, REFRESH_SKEW_MS, RefreshOauth as _,
        device::harness::{Reply, TestClock, serve},
        loopback::LoopbackError,
        now_ms,
        pkce::challenge_for,
        storage_key,
    },
    BrowserError, CALLBACK_PORT, CLIENT_ID, PROVIDER_ID, Refresh, SCOPE, XAI_USER_AGENT,
    browser_flow_at, credential_from, device_flow_at,
};

/// A canary that must never reach a message, a rendering or a log.
const REFRESH_CANARY: &str = "xai-refresh-canary-AAAA";

/// The authorization code a redirect hands back.
const CODE: &str = "ac-xai-8sJcqL41xTn0";

/// Longer than any test here spends waiting. Every outcome below is decided
/// by a request rather than by a clock, so this bound is never reached — it
/// only stops a broken build from hanging the suite.
const AMPLE: Duration = Duration::from_secs(60);

/// What both flows' token endpoints answer with, so that "the two store the
/// same bytes" is a claim about the code and not about two fixtures.
const TOKENS: &str =
    r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":7200,"token_type":"Bearer"}"#;

/// The value `name` has in a published authorize URL.
fn published(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .expect("a URL")
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("the authorize URL publishes no {name}: {url}"))
}

/// Sends one raw redirect to the login's listener and returns the whole
/// response, status line first.
///
/// Raw rather than through an HTTP client because half of what is asserted
/// is the status: a client that hid it behind an error type would not check
/// it.
async fn callback(port: u16, query: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the login is listening");
    socket
        .write_all(
            format!(
                "GET /callback?{query} HTTP/1.1\r\nHost: localhost\r\nConnection: \
                     close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("the redirect is written");

    let mut response = String::new();
    socket
        .read_to_string(&mut response)
        .await
        .expect("the response is read");

    response
}

/// A credential as it goes on disk, without the field that is a clock
/// reading.
///
/// The bytes are what matter: two flows that agree on every key and every
/// value are two flows a shared `auth.json` cannot tell apart. `expires` is
/// lifted out and asserted separately, because two logins run at two
/// instants and comparing those would be comparing the scheduler.
fn on_disk(credential: &OauthCredential) -> Value {
    let mut entry = credential.to_value();
    entry
        .as_object_mut()
        .expect("an entry is a JSON object")
        .remove("expires");

    entry
}

/// A credential holding the canary as its refresh token.
fn stored() -> OauthCredential {
    OauthCredential::new(
        SecretString::from(REFRESH_CANARY),
        SecretString::from("xai-access-canary-BBBB"),
        now_ms(),
    )
}

#[test]
fn a_login_goes_to_xais_own_endpoints_with_neither_swapped_for_the_other() {
    let flow = super::device_flow().expect("a client builds");

    // Not the constants asserted against themselves: what this catches is
    // the two being handed to `DeviceFlow::new` the wrong way round, which
    // compiles cleanly and fails only against the live provider.
    assert_eq!(
        flow.device_code_url(),
        "https://auth.x.ai/oauth2/device/code"
    );
    assert_eq!(flow.token_url(), "https://auth.x.ai/oauth2/token");
}

#[test]
fn ganja_calls_it_grok_and_the_file_calls_it_xai() {
    assert_eq!(PROVIDER_ID, "grok");
    assert_eq!(
        storage_key(PROVIDER_ID),
        "xai",
        "a shared auth.json only works if the key is the one upstream writes"
    );
}

#[tokio::test]
async fn a_login_asks_for_a_code_with_the_client_and_scope_xai_expects() {
    let endpoint = serve(vec![Reply::ok(
        r#"{"device_code":"dev","user_code":"WXYZ-1234",
                "verification_uri":"https://accounts.x.ai/device",
                "verification_uri_complete":"https://accounts.x.ai/device?code=WXYZ-1234",
                "interval":5,"expires_in":600}"#,
    )])
    .await;
    let flow = device_flow_at(
        format!("{}/device", endpoint.url),
        format!("{}/token", endpoint.url),
    )
    .expect("a client builds")
    .with_clock(TestClock::at(0));

    let started = flow
        .start(&tokio_util::sync::CancellationToken::new())
        .await
        .expect("the code is issued");

    let request = endpoint.request(0);
    assert_eq!(request.path(), "/device");
    assert!(
        request.has_header("content-type", "application/x-www-form-urlencoded"),
        "xAI's endpoints take the RFC's encoding, not JSON: {}",
        request.head
    );
    assert!(request.has_header("accept", "application/json"));
    // ganja's own product name since W4, against upstream's registered
    // client id — what that combination was probed against is this host's
    // constant's own doc. Asserted through that constant and again against
    // the name itself, so that repointing it is a decision somebody has to
    // come here and confirm. The version is deliberately not pinned: it
    // moves with every release of this crate, and pinning it here would
    // turn an ordinary version bump into a red test about identity.
    assert!(request.has_header("user-agent", XAI_USER_AGENT));
    let names_ganja = request.head.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("user-agent: ganja-code/")
    });
    assert!(
        names_ganja,
        "x.ai is told ganja's own name, not a borrowed one: {}",
        request.head
    );

    let fields = request.form();
    assert_eq!(fields.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(fields.get("scope").map(String::as_str), Some(SCOPE));
    assert!(
        SCOPE.contains("offline_access"),
        "without it the endpoint issues no refresh token at all"
    );

    assert_eq!(started.user_code, "WXYZ-1234");
    assert_eq!(
        started.browser_url(),
        "https://accounts.x.ai/device?code=WXYZ-1234",
        "the pre-filled page is the one to open where there is one"
    );
}

#[tokio::test]
async fn a_completed_login_becomes_a_credential_that_expires_when_xai_said() {
    let endpoint = serve(vec![
        Reply::ok(
            r#"{"device_code":"dev","user_code":"WXYZ",
                    "verification_uri":"https://accounts.x.ai/device","interval":5,
                    "expires_in":600}"#,
        ),
        Reply::ok(
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":7200,
                    "token_type":"Bearer"}"#,
        ),
    ])
    .await;
    let flow = device_flow_at(
        format!("{}/device", endpoint.url),
        format!("{}/token", endpoint.url),
    )
    .expect("a client builds");
    let cancel = tokio_util::sync::CancellationToken::new();

    let started = flow.start(&cancel).await.expect("the code is issued");
    let before = now_ms();
    let credential = credential_from(&flow.poll(&started, &cancel).await.expect("it lands"));
    let after = now_ms();

    assert_eq!(credential.access.expose_secret(), "at-1");
    assert_eq!(credential.refresh.expose_secret(), "rt-1");
    assert!(
        (before + 7_200_000..=after + 7_200_000).contains(&credential.expires),
        "the expiry is now plus what the endpoint said, got {}",
        credential.expires
    );
    assert!(
        !credential.needs_refresh(now_ms(), REFRESH_SKEW_MS),
        "a token good for two hours is not due"
    );

    let poll = endpoint.request(1);
    let fields = poll.form();
    assert_eq!(
        fields.get("grant_type").map(String::as_str),
        Some("urn:ietf:params:oauth:grant-type:device_code")
    );
    assert_eq!(fields.get("device_code").map(String::as_str), Some("dev"));
    assert_eq!(fields.get("client_id").map(String::as_str), Some(CLIENT_ID));
}

#[test]
fn a_login_with_no_stated_lifetime_gets_upstreams_hour() {
    let credential = credential_from(&super::Tokens {
        access: SecretString::from("at-1"),
        refresh: Some(SecretString::from("rt-1")),
        expires_in: None,
    });

    assert!(
        credential.expires >= now_ms() + 3_500_000,
        "upstream falls back to an hour (`expires_in ?? 3600`), got {}",
        credential.expires
    );
}

#[tokio::test]
async fn a_renewal_stores_the_rotated_refresh_token() {
    let endpoint = serve(vec![Reply::ok(
        r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600}"#,
    )])
    .await;
    let refresher = Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");

    let renewed = refresher
        .refresh(PROVIDER_ID, &stored())
        .await
        .expect("the endpoint renewed it");

    assert_eq!(renewed.access.expose_secret(), "at-2");
    assert_eq!(
        renewed.refresh.expose_secret(),
        "rt-2",
        "xAI rotates, and the spent one must not be stored back"
    );

    let sent = endpoint.request(0).form();
    assert_eq!(
        sent.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        sent.get("refresh_token").map(String::as_str),
        Some(REFRESH_CANARY)
    );
    assert_eq!(sent.get("client_id").map(String::as_str), Some(CLIENT_ID));
}

#[tokio::test]
async fn a_renewal_that_rotates_nothing_keeps_the_token_it_presented() {
    let endpoint = serve(vec![Reply::ok(
        r#"{"access_token":"at-2","expires_in":3600}"#,
    )])
    .await;
    let refresher = Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");

    let renewed = refresher
        .refresh(PROVIDER_ID, &stored())
        .await
        .expect("the endpoint renewed it");

    assert_eq!(
        renewed.refresh.expose_secret(),
        REFRESH_CANARY,
        "an endpoint that sent no new refresh token has not revoked the old one \
             (`xai.ts:500`)"
    );
}

#[tokio::test]
async fn a_refused_refresh_token_asks_for_a_new_login() {
    let endpoint = serve(vec![Reply::new(
            401,
            // The shape that makes this worth testing: the endpoint quotes the
            // token it refused.
            format!(
                r#"{{"error":"invalid_grant","error_description":"refresh token {REFRESH_CANARY} is not valid"}}"#
            ),
        )])
        .await;
    let refresher = Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");

    let failure = refresher
        .refresh(PROVIDER_ID, &stored())
        .await
        .expect_err("a refused token is not a credential");

    assert_eq!(failure.kind(), AuthErrorKind::ReauthRequired);
    let rendered = format!("{failure} {failure:?}");
    assert!(
        rendered.contains("401") && rendered.contains("invalid_grant"),
        "the status and the code are what a person acts on: {rendered}"
    );
    assert!(
        !rendered.contains(REFRESH_CANARY),
        "the echoed token reached the message: {rendered}"
    );
    assert!(
        !rendered.contains("error_description") && !rendered.contains("not valid"),
        "the body must not travel into a message that will be logged: {rendered}"
    );
}

#[tokio::test]
async fn a_token_endpoint_that_broke_is_not_a_dead_credential() {
    for status in [500, 502, 503] {
        let endpoint = serve(vec![Reply::new(status, r#"{"error":"server_error"}"#)]).await;
        let refresher = Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");

        let failure = refresher
            .refresh(PROVIDER_ID, &stored())
            .await
            .expect_err("a broken endpoint renewed nothing");

        assert_eq!(
            failure.kind(),
            AuthErrorKind::RefreshUnavailable,
            "{status} is the endpoint being broken, not the token being dead"
        );
    }
}

#[tokio::test]
async fn a_token_endpoint_that_cannot_be_reached_leaves_the_credential_alone() {
    // A listener with nothing to serve stops listening at once, so the
    // connection is refused rather than answered.
    let endpoint = serve(Vec::new()).await;
    let refresher = Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");

    let failure = refresher
        .refresh(PROVIDER_ID, &stored())
        .await
        .expect_err("nothing answered");

    assert_eq!(failure.kind(), AuthErrorKind::RefreshUnavailable);
    let rendered = format!("{failure} {failure:?}");
    assert!(
        rendered.contains("still good"),
        "the remedy is to try again, not to log in: {rendered}"
    );
    assert!(!rendered.contains(REFRESH_CANARY), "{rendered}");
}

#[tokio::test]
async fn a_credential_with_no_refresh_token_says_so_without_a_round_trip() {
    let endpoint = serve(vec![Reply::ok(r#"{"access_token":"never"}"#)]).await;
    let refresher = Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");
    let credential = OauthCredential::new(
        SecretString::from("   "),
        SecretString::from("at-1"),
        now_ms(),
    );

    let failure = refresher
        .refresh(PROVIDER_ID, &credential)
        .await
        .expect_err("there is nothing to present");

    assert_eq!(failure.kind(), AuthErrorKind::ReauthRequired);
    assert_eq!(
        endpoint.count(),
        0,
        "presenting nothing to the endpoint can only come back as the same answer"
    );
    assert!(matches!(failure, AuthError::ReauthRequired { .. }));
}

#[tokio::test]
async fn an_authorize_url_carries_every_parameter_xai_requires() {
    let flow = browser_flow_at(
        "https://authorize.invalid/oauth2/authorize",
        "https://token.invalid",
    )
    .expect("a client builds");
    let url = flow.authorize_url(
        "http://127.0.0.1:56121/callback",
        "the-challenge",
        "the-state",
        "the-nonce",
    );
    let query: Vec<(String, String)> = url::Url::parse(&url)
        .expect("a URL")
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    assert!(
        url.starts_with("https://authorize.invalid/oauth2/authorize?"),
        "{url}"
    );
    // The whole list, in upstream's own insertion order (`xai.ts:129-141`).
    // Asserted as a vector rather than pair by pair so that a parameter
    // going missing is a failure rather than an assertion nobody wrote:
    // without `plan=generic` the account portal refuses this client
    // outright, and the only place that can be caught before a person meets
    // it in a browser is here.
    assert_eq!(
        query,
        vec![
            ("response_type".to_owned(), "code".to_owned()),
            (
                "client_id".to_owned(),
                "b1a00492-073a-47ea-816f-4c329264a828".to_owned()
            ),
            (
                "redirect_uri".to_owned(),
                "http://127.0.0.1:56121/callback".to_owned()
            ),
            (
                "scope".to_owned(),
                "openid profile email offline_access grok-cli:access api:access".to_owned()
            ),
            ("code_challenge".to_owned(), "the-challenge".to_owned()),
            ("code_challenge_method".to_owned(), "S256".to_owned()),
            ("state".to_owned(), "the-state".to_owned()),
            ("nonce".to_owned(), "the-nonce".to_owned()),
            ("plan".to_owned(), "generic".to_owned()),
            ("referrer".to_owned(), "ganja-code".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_browser_login_asks_xai_to_redirect_to_the_address_it_registered() {
    // Every other test here binds port 0, so the two constants a production
    // login actually uses are otherwise unread — and both belong to a client
    // registration this project cannot change. Upstream's own comment
    // (`xai.ts:33-35`): the host:port pair is part of the registration.
    assert_eq!(CALLBACK_PORT, 56121, "xai.ts:37");

    let flow = super::browser_flow().expect("a client builds");
    let url = flow.authorize_url("http://127.0.0.1:56121/callback", "c", "s", "n");

    assert!(
        url.starts_with("https://auth.x.ai/oauth2/authorize?"),
        "xai.ts:11, got {url}"
    );
}

#[tokio::test]
async fn the_challenge_a_browser_login_publishes_is_the_digest_of_the_verifier_it_kept() {
    let flow = browser_flow_at("https://authorize.invalid", "https://token.invalid")
        .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");

    assert_eq!(
        published(browser.url(), "code_challenge"),
        challenge_for(browser.pkce.verifier().expose_secret()),
        "xAI recomputes this over the verifier the exchange presents, so a challenge \
             that is not the S256 of it fails at the very end of a flow a person has \
             already completed in a browser"
    );
    assert_eq!(published(browser.url(), "code_challenge_method"), "S256");
    assert_eq!(
        published(browser.url(), "redirect_uri"),
        format!("http://127.0.0.1:{}/callback", browser.port()),
        "the redirect has to name the port that was actually bound"
    );
    assert_ne!(browser.port(), 0, "a bound socket has a real port");
    assert_ne!(
        browser.port(),
        CALLBACK_PORT,
        "port 0 is what keeps a test off the registered port"
    );
}

#[tokio::test]
async fn a_browser_login_draws_a_state_and_a_nonce_that_are_not_the_same_value() {
    let flow = browser_flow_at("https://authorize.invalid", "https://token.invalid")
        .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");

    let state = published(browser.url(), "state");
    let nonce = published(browser.url(), "nonce");

    assert!(!state.is_empty() && !nonce.is_empty());
    assert_ne!(
        state, nonce,
        "one value drawn once and spent twice is one value, whatever it is called"
    );
    assert_eq!(
        state,
        browser.state.expose_secret(),
        "the value published is the value the callback will be checked against"
    );
}

#[tokio::test]
async fn a_browser_login_stores_the_credential_a_device_login_stores() {
    let exchange = serve(vec![Reply::ok(TOKENS)]).await;
    let flow = browser_flow_at(
        format!("{}/authorize", exchange.url),
        format!("{}/token", exchange.url),
    )
    .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");
    let port = browser.port();
    let state = published(browser.url(), "state");
    let challenge = published(browser.url(), "code_challenge");

    let cancel = CancellationToken::new();
    let driven = tokio::spawn({
        let cancel = cancel.clone();
        async move { browser.wait(AMPLE, &cancel).await }
    });

    let response = callback(port, &format!("code={CODE}&state={state}")).await;
    let tokens = driven
        .await
        .expect("the wait finished")
        .expect("the callback was accepted and exchanged");

    assert_eq!(response.lines().next(), Some("HTTP/1.1 200 OK"));

    let sent = exchange.request(0);
    assert_eq!(sent.path(), "/token");
    assert!(
        sent.has_header("content-type", "application/x-www-form-urlencoded"),
        "xAI's token endpoint takes the RFC's encoding, not JSON: {}",
        sent.head
    );
    assert!(sent.has_header("accept", "application/json"));
    assert!(sent.has_header("user-agent", XAI_USER_AGENT));

    let fields = sent.form();
    assert_eq!(
        fields.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(fields.get("code").map(String::as_str), Some(CODE));
    assert_eq!(
        fields.get("redirect_uri").map(String::as_str),
        Some(format!("http://127.0.0.1:{port}/callback").as_str()),
        "RFC 6749 4.1.3 requires the exchange to repeat the redirect the \
             authorization named"
    );
    assert_eq!(fields.get("client_id").map(String::as_str), Some(CLIENT_ID));
    // The wire-level S256 pin: what was published in the browser's URL has
    // to be the digest of what is presented here, and this is the only
    // place both halves exist at once.
    assert_eq!(
        challenge,
        challenge_for(
            fields
                .get("code_verifier")
                .expect("the exchange presents a verifier")
        ),
        "a verifier that is not the preimage of the published challenge is refused \
             by the token endpoint, not by anything here"
    );

    // The device path, against its own endpoint answering the same tokens.
    let device_endpoint = serve(vec![
        Reply::ok(
            r#"{"device_code":"dev","user_code":"WXYZ",
                    "verification_uri":"https://accounts.x.ai/device","interval":5,
                    "expires_in":600}"#,
        ),
        Reply::ok(TOKENS),
    ])
    .await;
    let device = device_flow_at(
        format!("{}/device", device_endpoint.url),
        format!("{}/token", device_endpoint.url),
    )
    .expect("a client builds");
    let started = device.start(&cancel).await.expect("the code is issued");
    let device_tokens = device.poll(&started, &cancel).await.expect("it lands");

    let before = now_ms();
    let browsed = credential_from(&tokens);
    let device_grant = credential_from(&device_tokens);
    let after = now_ms();

    assert_eq!(
        on_disk(&browsed),
        on_disk(&device_grant),
        "the same key, the same tokens, the same absent fields — anything else and \
             a shared auth.json holds two shapes for one provider"
    );
    assert_eq!(browsed.access.expose_secret(), "at-1");
    assert_eq!(browsed.refresh.expose_secret(), "rt-1");
    assert!(
        (before + 7_200_000..=after + 7_200_000).contains(&browsed.expires),
        "the expiry is now plus what the endpoint said, got {}",
        browsed.expires
    );
    assert_eq!(
        storage_key(PROVIDER_ID),
        "xai",
        "and both are filed under the name upstream writes"
    );
}

#[tokio::test]
async fn the_state_a_login_publishes_is_the_one_its_callback_must_echo() {
    // The narrow claim the whole browser flow rests on: what went into the
    // URL is what the listener is told to check. A login that published one
    // value and validated another would refuse its own callback, minutes
    // into a flow somebody already finished in a browser.
    let exchange = serve(vec![Reply::ok(TOKENS)]).await;
    let flow = browser_flow_at(
        format!("{}/authorize", exchange.url),
        format!("{}/token", exchange.url),
    )
    .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");
    let port = browser.port();
    let state = published(browser.url(), "state");

    let cancel = CancellationToken::new();
    let driven = tokio::spawn(async move { browser.wait(AMPLE, &cancel).await });

    callback(port, &format!("code={CODE}&state={state}")).await;

    assert_eq!(
        driven
            .await
            .expect("the wait finished")
            .expect("a callback echoing the published state belongs to this login")
            .access
            .expose_secret(),
        "at-1"
    );
}

#[tokio::test]
async fn a_forged_callback_is_refused_before_its_error_parameter_is_read() {
    // Upstream reads `error` first and compares `state` only afterwards
    // (`xai.ts:332-341` against `:352`), which means a redirect nobody could
    // have sent still decides what a person is told. Inherited from
    // `loopback` rather than re-decided here, and asserted through this flow
    // so the inheritance is a fact rather than an assumption.
    let exchange = serve(Vec::new()).await;
    let flow = browser_flow_at(
        format!("{}/authorize", exchange.url),
        format!("{}/token", exchange.url),
    )
    .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");
    let port = browser.port();

    let cancel = CancellationToken::new();
    let driven = tokio::spawn(async move { browser.wait(AMPLE, &cancel).await });

    let response = callback(
        port,
        &format!("error=access_denied&error_description=user+said+no&code={CODE}&state=theirs"),
    )
    .await;
    let refused = driven
        .await
        .expect("the wait finished")
        .expect_err("a callback that proves nothing must not be accepted");

    assert!(
        matches!(
            &refused,
            BrowserError::Callback {
                source: LoopbackError::Forged
            }
        ),
        "{refused:?}"
    );
    let message = format!("{refused} {refused:?}");
    assert!(
        !message.contains("access_denied") && !message.contains("user said no"),
        "a value out of a request that could not prove it belongs here was read \
             anyway: {message}"
    );
    assert_eq!(response.lines().next(), Some("HTTP/1.1 400 Bad Request"));
    assert_eq!(
        exchange.count(),
        0,
        "a forged callback must not cost a request to the token endpoint"
    );
}

#[tokio::test]
async fn a_callback_that_gives_the_state_twice_is_refused() {
    let exchange = serve(Vec::new()).await;
    let flow = browser_flow_at(
        format!("{}/authorize", exchange.url),
        format!("{}/token", exchange.url),
    )
    .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");
    let port = browser.port();
    let state = published(browser.url(), "state");

    let cancel = CancellationToken::new();
    let driven = tokio::spawn(async move { browser.wait(AMPLE, &cancel).await });

    // One of the two is this login's own, so a parser that took either end
    // of the query would accept it.
    let response = callback(port, &format!("code={CODE}&state={state}&state=theirs")).await;
    let refused = driven
        .await
        .expect("the wait finished")
        .expect_err("a value two parties disagree about was not given");

    assert!(
        matches!(
            &refused,
            BrowserError::Callback {
                source: LoopbackError::Forged
            }
        ),
        "{refused:?}"
    );
    assert_eq!(response.lines().next(), Some("HTTP/1.1 400 Bad Request"));
    assert_eq!(exchange.count(), 0);
}

#[tokio::test]
async fn a_browser_login_whose_port_is_taken_names_the_two_ways_out() {
    // The registered port is not negotiable, so the only useful thing to say
    // is which two things a person can do about it. Bound here on an
    // OS-assigned port rather than on 56121, so the test neither contends
    // with a parallel runner nor with whatever is running on this machine.
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let taken = holder
        .local_addr()
        .expect("a bound socket has an address")
        .port();
    let flow = browser_flow_at("https://authorize.invalid", "https://token.invalid")
        .expect("a client builds");

    let refused = flow
        .start_on(taken)
        .await
        .expect_err("the port is already held");

    assert!(
        matches!(&refused, BrowserError::PortTaken { port, .. } if *port == taken),
        "{refused:?}"
    );
    let message = refused.to_string();
    assert!(message.contains(&taken.to_string()), "{message}");
    assert!(
        message.contains("device method"),
        "the other login is the way out that always works: {message}"
    );
    assert!(
        message.contains("close"),
        "and freeing the port is the other: {message}"
    );
}

#[tokio::test]
async fn nothing_renders_a_state_or_a_verifier_out_of_a_browser_login() {
    // A login in flight is held for minutes, which is exactly the window in
    // which a `tracing` field or somebody's `{:?}` would put its secrets in
    // a log. What is asserted is about the type this module hands a caller,
    // not about `SecretString`.
    let flow = browser_flow_at("https://authorize.invalid", "https://token.invalid")
        .expect("a client builds");
    let browser = flow.start_on(0).await.expect("loopback is bindable");
    let rendered = format!("{browser:?}");

    assert!(
        !rendered.contains(&published(browser.url(), "state")),
        "the value that decides whose callback is accepted reached a Debug: {rendered}"
    );
    assert!(
        !rendered.contains(&published(browser.url(), "nonce")),
        "{rendered}"
    );
    assert!(
        !rendered.contains(browser.pkce.verifier().expose_secret()),
        "a verifier reached a Debug: {rendered}"
    );
    // The challenge is published in that same URL, so its presence is what
    // keeps the assertions above from being vacuous.
    assert!(rendered.contains(browser.pkce.challenge()), "{rendered}");
}
