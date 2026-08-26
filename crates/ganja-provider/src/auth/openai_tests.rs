use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::ExposeSecret as _;
use serde_json::json;

use super::{
    CALLBACK_PORT, DEFAULT_POLL_SECONDS, Device, Login, LoginError, PROVIDER_ID, Tokens,
    claimed_account, is_dead_grant, poll_interval,
};
use crate::auth::{AuthErrorKind, pkce};

/// An account id no other value in a test could be mistaken for.
const ACCOUNT: &str = "acct_2f7QpL9";

/// A token that must never appear in anything a person or a log reads.
const CANARY: &str = "sk-canary-DO-NOT-PRINT-8891";

/// A JWT carrying `claims`, signed with nothing at all.
///
/// The signature is deliberately not a signature: every test here is about
/// a value read out of a token that was never verified, and a fixture that
/// looked signed would suggest otherwise.
fn token(claims: &serde_json::Value) -> String {
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());

    format!("eyJhbGciOiJSUzI1NiJ9.{payload}.not-a-signature")
}

#[test]
fn an_authorize_url_carries_every_parameter_the_issuer_requires() {
    let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
    let url = login.authorize_url("http://localhost:1455/auth/callback", "the-challenge", "st");
    let query: Vec<(String, String)> = url::Url::parse(&url)
        .expect("a URL")
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    assert!(
        url.starts_with("https://issuer.invalid/oauth/authorize?"),
        "{url}"
    );
    assert_eq!(
        query,
        vec![
            ("response_type".to_owned(), "code".to_owned()),
            (
                "client_id".to_owned(),
                "app_EMoamEEZ73f0CkXaXp7hrann".to_owned()
            ),
            (
                "redirect_uri".to_owned(),
                "http://localhost:1455/auth/callback".to_owned()
            ),
            (
                "scope".to_owned(),
                "openid profile email offline_access".to_owned()
            ),
            ("code_challenge".to_owned(), "the-challenge".to_owned()),
            ("code_challenge_method".to_owned(), "S256".to_owned()),
            ("id_token_add_organizations".to_owned(), "true".to_owned()),
            ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
            ("state".to_owned(), "st".to_owned()),
            ("originator".to_owned(), "ganja-code".to_owned()),
        ]
    );
}

#[tokio::test]
async fn the_challenge_a_login_publishes_is_the_digest_of_the_verifier_it_kept() {
    let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
    let browser = login.browser_on(0).await.expect("loopback is bindable");

    let published = url::Url::parse(browser.url())
        .expect("a URL")
        .query_pairs()
        .find(|(key, _)| key == "code_challenge")
        .map(|(_, value)| value.into_owned())
        .expect("the authorize URL publishes a challenge");

    assert_eq!(
        published,
        pkce::challenge_for(browser.pkce.verifier().expose_secret()),
        "the issuer recomputes this over the verifier the exchange presents"
    );
    assert_ne!(browser.port(), 0, "a bound socket has a real port");
}

#[tokio::test]
async fn a_browser_login_names_the_port_it_is_actually_listening_on() {
    let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
    let browser = login.browser_on(0).await.expect("loopback is bindable");

    let redirect = url::Url::parse(browser.url())
        .expect("a URL")
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .expect("the authorize URL names a redirect");

    assert_eq!(
        redirect,
        format!("http://localhost:{}/auth/callback", browser.port())
    );
    assert_ne!(
        browser.port(),
        CALLBACK_PORT,
        "port 0 is what keeps a test off the registered port"
    );
}

#[test]
fn an_issuer_that_would_put_the_tokens_in_the_clear_is_refused() {
    for allowed in [
        "https://auth.openai.com",
        "http://127.0.0.1:8080",
        "http://localhost:9",
    ] {
        assert!(Login::with_issuer(allowed).is_ok(), "{allowed}");
    }
    for refused in [
        "http://auth.openai.com",
        "http://127.0.0.1.invalid",
        "http://localhost.invalid",
        "ftp://auth.openai.com",
        "not a url",
    ] {
        assert!(
            matches!(Login::with_issuer(refused), Err(LoginError::Issuer)),
            "{refused}"
        );
    }
}

#[test]
fn an_issuer_carrying_a_secret_is_refused_before_it_can_reach_a_browser() {
    // The authorize URL is built by prefixing the issuer, and that URL is
    // printed to a terminal and opened in a browser; `reqwest` would also
    // turn userinfo into a `Basic` header on every token request. Both are
    // closed by refusing the issuer rather than by redacting it later.
    for carrying in [
        &format!("https://{CANARY}@issuer.invalid"),
        &format!("https://user:{CANARY}@issuer.invalid"),
        &format!("https://issuer.invalid?token={CANARY}"),
        &format!("https://issuer.invalid#{CANARY}"),
    ] {
        assert!(
            matches!(Login::with_issuer(carrying), Err(LoginError::Issuer)),
            "{carrying}"
        );
    }
}

#[test]
fn a_trailing_slash_on_the_issuer_does_not_double_up_in_a_path() {
    let login = Login::with_issuer("https://issuer.invalid/").expect("https is allowed");

    assert!(
        login
            .authorize_url("http://localhost:1/cb", "c", "s")
            .starts_with("https://issuer.invalid/oauth/authorize?"),
        "{}",
        login.authorize_url("http://localhost:1/cb", "c", "s")
    );
}

#[test]
fn an_account_id_is_read_from_each_claim_shape_in_priority_order() {
    // The flat claim wins over the namespaced one, which wins over the
    // first organization — upstream's own `??` chain (`openai.ts:284-288`).
    let all_three = json!({
        "chatgpt_account_id": ACCOUNT,
        "https://api.openai.com/auth": { "chatgpt_account_id": "namespaced" },
        "organizations": [{ "id": "organization" }],
    });
    assert_eq!(
        claimed_account(&token(&all_three)).as_deref(),
        Some(ACCOUNT)
    );

    let namespaced = json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": ACCOUNT },
        "organizations": [{ "id": "organization" }],
    });
    assert_eq!(
        claimed_account(&token(&namespaced)).as_deref(),
        Some(ACCOUNT)
    );

    let organizations = json!({ "organizations": [{ "id": ACCOUNT }, { "id": "second" }] });
    assert_eq!(
        claimed_account(&token(&organizations)).as_deref(),
        Some(ACCOUNT)
    );
}

#[test]
fn a_token_nobody_could_verify_still_yields_its_account_id() {
    // The signature here is the string "not-a-signature". Nothing checks
    // it, and nothing should read this as though something did.
    let unverifiable = token(&json!({ "chatgpt_account_id": ACCOUNT }));

    assert!(unverifiable.ends_with(".not-a-signature"));
    assert_eq!(claimed_account(&unverifiable).as_deref(), Some(ACCOUNT));
}

#[test]
fn a_padded_payload_decodes_like_an_unpadded_one() {
    // JWT producers differ on whether they pad. Refusing a padded payload
    // would be strictness with no security value on a value already not
    // trusted.
    let claims = json!({ "chatgpt_account_id": ACCOUNT });
    let payload = base64::engine::general_purpose::URL_SAFE.encode(claims.to_string());

    assert!(payload.ends_with('='), "the fixture has to be padded");
    assert_eq!(
        claimed_account(&format!("header.{payload}.signature")).as_deref(),
        Some(ACCOUNT)
    );
}

#[test]
fn a_token_with_no_account_in_it_is_not_a_failure() {
    for shapeless in [
        token(&json!({ "email": "someone@example.invalid" })),
        token(&json!({ "organizations": [] })),
        token(&json!("not an object")),
        "two.segments".to_owned(),
        "four.segments.than.expected".to_owned(),
        "not-a-jwt".to_owned(),
        "header.!!!not-base64!!!.signature".to_owned(),
        String::new(),
    ] {
        assert_eq!(claimed_account(&shapeless), None, "{shapeless}");
    }
}

#[test]
fn a_login_with_no_account_id_is_still_a_login() {
    let tokens: Tokens = serde_json::from_value(json!({
        "access_token": "at-1", "refresh_token": "rt-1", "id_token": "not-a-jwt",
    }))
    .expect("the shape decodes");
    let credential = super::first_credential(tokens, "testing").expect("a login");

    assert_eq!(credential.account_id, None);
    assert_eq!(credential.access.expose_secret(), "at-1");
}

#[test]
fn a_login_whose_answer_carries_no_refresh_token_is_not_a_login() {
    // There would be no way back from the hour it lasts.
    let tokens: Tokens =
        serde_json::from_value(json!({ "access_token": "at-1" })).expect("the shape decodes");

    assert!(matches!(
        super::first_credential(tokens, "testing"),
        Err(LoginError::Malformed { .. })
    ));
}

#[test]
fn only_a_4xx_that_is_not_a_rate_limit_means_logging_in_again() {
    for dead in [400, 401, 403, 404, 422, 499] {
        assert!(is_dead_grant(dead), "{dead}");
    }
    for survivable in [200, 429, 500, 502, 503] {
        assert!(!is_dead_grant(survivable), "{survivable}");
    }
}

#[test]
fn a_refusal_and_a_transport_failure_are_never_the_same_answer() {
    let refused = LoginError::Refused {
        step: "renewing the credential",
        status: 401,
        code: "invalid_grant".to_owned(),
    }
    .into_auth(PROVIDER_ID);
    assert_eq!(refused.kind(), AuthErrorKind::ReauthRequired);
    assert!(refused.to_string().contains("invalid_grant"), "{refused}");

    let limited = LoginError::Refused {
        step: "renewing the credential",
        status: 429,
        code: "rate_limit_exceeded".to_owned(),
    }
    .into_auth(PROVIDER_ID);
    assert_eq!(limited.kind(), AuthErrorKind::RefreshUnavailable);

    let malformed = LoginError::Malformed {
        step: "renewing the credential",
    }
    .into_auth(PROVIDER_ID);
    assert_eq!(malformed.kind(), AuthErrorKind::RefreshUnavailable);

    let cancelled = LoginError::Cancelled.into_auth(PROVIDER_ID);
    assert_eq!(cancelled.kind(), AuthErrorKind::RefreshUnavailable);
}

#[test]
fn no_failure_message_renders_a_token() {
    let messages = [
        LoginError::Refused {
            step: "renewing the credential",
            status: 401,
            // What a careless implementation would put here: the body.
            code: super::UNNAMED.to_owned(),
        }
        .into_auth(PROVIDER_ID)
        .to_string(),
        LoginError::Malformed {
            step: "exchanging the authorization code",
        }
        .into_auth(PROVIDER_ID)
        .to_string(),
        // A `Login` is held for the length of a login and lands in a
        // `tracing` field the moment anybody adds one, so its own rendering
        // has to be safe too. It is, because nothing it holds may carry a
        // secret — see the issuer check.
        format!(
            "{:?}",
            Login::with_issuer("https://issuer.invalid").expect("https is allowed")
        ),
    ];

    for message in messages {
        assert!(
            !message.contains(CANARY),
            "a secret reached a message: {message}"
        );
    }
}

#[test]
fn the_poll_interval_is_the_one_the_issuer_named() {
    assert_eq!(
        poll_interval(Some(&json!("7"))),
        Duration::from_secs(7),
        "the field arrives as a string"
    );
    assert_eq!(
        poll_interval(Some(&json!(7))),
        Duration::from_secs(7),
        "and a number is not a reason to fail a login"
    );
    for absent in [
        None,
        Some(&json!("0")),
        Some(&json!("")),
        Some(&json!(null)),
    ] {
        assert_eq!(
            poll_interval(absent),
            Duration::from_secs(DEFAULT_POLL_SECONDS),
            "{absent:?}"
        );
    }
}

#[tokio::test]
async fn nothing_renders_a_state_or_a_verifier_out_of_a_started_login() {
    // A login in flight is held for minutes, which is exactly the window in
    // which a `tracing` field or somebody's `{:?}` would put its secrets in
    // a log. Asserting that `SecretString` redacts would prove something
    // about the `secrecy` crate; this asserts it about the two types this
    // module hands a caller.
    let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
    let browser = login.browser_on(0).await.expect("loopback is bindable");
    let rendered = format!("{browser:?}");

    let state = url::Url::parse(browser.url())
        .expect("a URL")
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("the authorize URL publishes a state");

    assert!(
        !rendered.contains(&state),
        "the value that decides whose callback is accepted reached a Debug: {rendered}"
    );
    assert!(
        !rendered.contains(browser.pkce.verifier().expose_secret()),
        "a verifier reached a Debug: {rendered}"
    );
    // The challenge is published in the same URL, so its presence is what
    // keeps the two assertions above from being vacuous.
    assert!(rendered.contains(browser.pkce.challenge()), "{rendered}");

    let device = Device {
        login,
        url: "https://issuer.invalid/codex/device".to_owned(),
        user_code: "ABCD-EFGH".to_owned(),
        device_auth_id: CANARY.to_owned(),
        interval: Duration::from_secs(5),
    };
    let rendered = format!("{device:?}");

    assert!(
        !rendered.contains(CANARY),
        "half of what claims the grant reached a Debug: {rendered}"
    );
    // The code is on a screen by design; showing it is what makes this
    // assertion about the *other* half rather than about redacting whatever
    // is nearest.
    assert!(rendered.contains("ABCD-EFGH"), "{rendered}");
}

#[test]
fn the_registered_client_is_the_one_a_plain_login_talks_to() {
    // Every other test here points at an issuer of its own, so the two
    // constants a production login actually uses are otherwise unread — and
    // both belong to a client registration this project cannot change. A
    // typo in either fails nowhere but in front of a person.
    let login = Login::new().expect("the real issuer is https");

    assert!(
        login
            .authorize_url("http://localhost:1455/auth/callback", "c", "s")
            .starts_with("https://auth.openai.com/oauth/authorize?"),
        "openai.ts:15"
    );
    assert_eq!(CALLBACK_PORT, 1455, "openai.ts:16");
}
