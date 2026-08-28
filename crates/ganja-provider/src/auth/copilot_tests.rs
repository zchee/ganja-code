use secrecy::{ExposeSecret as _, SecretString};

use super::super::REFRESH_SKEW_MS;
use super::super::device::harness::{Reply, TestClock, serve};
use super::super::device::{Tokens, UPSTREAM_USER_AGENT};
use super::{
    API_VERSION, CLIENT_ID, DEFAULT_API_BASE, Deployment, api_base_for, credential_from,
    device_flow, device_flow_at, normalize_domain,
};

/// A token that must never render whole.
const TOKEN_CANARY: &str = "gho_copilot-canary-CCCC";

#[test]
fn every_way_of_spelling_an_enterprise_domain_names_the_same_deployment() {
    for spelling in [
        "company.ghe.com",
        "https://company.ghe.com",
        "https://company.ghe.com/",
        "http://company.ghe.com",
        // Not upstream's case, but the one its single-slash regex would
        // leave a slash on.
        "https://company.ghe.com//",
    ] {
        assert_eq!(
            normalize_domain(spelling),
            "company.ghe.com",
            "{spelling} names company.ghe.com"
        );
        assert_eq!(
            api_base_for(Deployment::enterprise(spelling).domain()),
            "https://copilot-api.company.ghe.com",
            "{spelling} should reach the enterprise API base"
        );
        assert_eq!(
            Deployment::enterprise(spelling).enterprise_url(),
            Some("company.ghe.com"),
            "{spelling} is what gets stored beside the token"
        );
    }
}

#[test]
fn the_public_deployment_reaches_githubs_own_api_base() {
    assert_eq!(Deployment::Public.domain(), "github.com");
    assert_eq!(api_base_for(Deployment::Public.domain()), DEFAULT_API_BASE);
    assert_eq!(
        Deployment::Public.enterprise_url(),
        None,
        "there is no enterprise URL to store for github.com"
    );
    assert_eq!(api_base_for("github.com"), DEFAULT_API_BASE);
    assert_eq!(api_base_for("https://github.com/"), DEFAULT_API_BASE);
    assert_eq!(
        API_VERSION, "2026-06-01",
        "the version every Copilot request declares (`copilot.ts:10`)"
    );
}

#[test]
fn a_login_goes_to_the_deployment_it_is_for() {
    let public = device_flow(&Deployment::Public).expect("a client builds");
    assert_eq!(public.device_code_url(), "https://github.com/login/device/code");
    assert_eq!(public.token_url(), "https://github.com/login/oauth/access_token");

    // The enterprise login talks to the enterprise host, not to
    // github.com — the API base is derived separately (`copilot.ts:19-28`)
    // and the two must not be confused for each other.
    let enterprise =
        device_flow(&Deployment::enterprise("https://company.ghe.com/")).expect("it builds");
    assert_eq!(enterprise.device_code_url(), "https://company.ghe.com/login/device/code");
    assert_eq!(enterprise.token_url(), "https://company.ghe.com/login/oauth/access_token");
}

#[tokio::test]
async fn a_login_asks_github_for_a_code_in_the_json_body_it_wants() {
    let endpoint = serve(vec![Reply::ok(
        r#"{"device_code":"dev","user_code":"ABCD-1234",
                "verification_uri":"https://github.com/login/device","interval":5}"#,
    )])
    .await;
    let flow = device_flow_at(
        format!("{}/login/device/code", endpoint.url),
        format!("{}/login/oauth/access_token", endpoint.url),
    )
    .expect("a client builds")
    .with_clock(TestClock::at(0));

    let started =
        flow.start(&tokio_util::sync::CancellationToken::new()).await.expect("the code is issued");

    let request = endpoint.request(0);
    assert_eq!(request.path(), "/login/device/code");
    // The three headers upstream sends (`copilot.ts:236-240`). The content
    // type is the divergence worth pinning: everything else in this build
    // that speaks a device flow sends a form.
    assert!(
        request.has_header("content-type", "application/json"),
        "GitHub's device endpoints take JSON: {}",
        request.head
    );
    assert!(request.has_header("accept", "application/json"));
    // Upstream's own product name, against upstream's own registered
    // client id — the combination the live spikes measured. Asserted as a
    // literal as well as through the constant, so that changing the
    // constant is a decision somebody has to come here and confirm.
    assert!(request.has_header("user-agent", UPSTREAM_USER_AGENT));
    assert!(request.has_header("user-agent", "opencode/1.18.22"));

    assert_eq!(
        request.json(),
        serde_json::json!({"client_id": CLIENT_ID, "scope": "read:user"}),
        "the body is the object upstream sends, and nothing more"
    );
    assert_eq!(started.user_code, "ABCD-1234");
    assert_eq!(
        started.browser_url(),
        "https://github.com/login/device",
        "GitHub sends no pre-filled page, so the plain one is what to open"
    );
}

#[tokio::test]
async fn a_completed_login_stores_one_token_twice_and_never_expires() {
    let endpoint = serve(vec![
        Reply::ok(
            r#"{"device_code":"dev","user_code":"ABCD","interval":5,
                    "verification_uri":"https://company.ghe.com/login/device"}"#,
        ),
        // GitHub's spelling of "not yet": a 200 carrying the error.
        Reply::ok(r#"{"error":"authorization_pending"}"#),
        Reply::ok(format!(r#"{{"access_token":"{TOKEN_CANARY}"}}"#)),
    ])
    .await;
    let deployment = Deployment::enterprise("https://company.ghe.com/");
    let flow = device_flow_at(
        format!("{}/login/device/code", endpoint.url),
        format!("{}/login/oauth/access_token", endpoint.url),
    )
    .expect("a client builds")
    .with_clock(TestClock::at(0));
    let cancel = tokio_util::sync::CancellationToken::new();

    let started = flow.start(&cancel).await.expect("the code is issued");
    let credential =
        credential_from(&flow.poll(&started, &cancel).await.expect("the login lands"), &deployment);

    assert_eq!(credential.access.expose_secret(), TOKEN_CANARY);
    assert_eq!(
        credential.refresh.expose_secret(),
        TOKEN_CANARY,
        "there is one token, and every request reads it out of `refresh`"
    );
    assert_eq!(credential.enterprise_url.as_deref(), Some("company.ghe.com"));

    let poll = endpoint.request(2).json();
    assert_eq!(
        poll,
        serde_json::json!({
            "client_id": CLIENT_ID,
            "device_code": "dev",
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        })
    );
    assert!(
        !format!("{credential:?} {credential}").contains(TOKEN_CANARY),
        "the token must not render whole"
    );
}

#[test]
fn a_copilot_credential_is_never_due_for_a_renewal_it_has_no_way_to_do() {
    let credential = credential_from(
        &Tokens { access: SecretString::from(TOKEN_CANARY), refresh: None, expires_in: None },
        &Deployment::Public,
    );

    assert_eq!(credential.expires, 0, "`expires: 0` is upstream's `never` (`copilot.ts:299`)");
    // Every clock a caller could ask with, including one long past any
    // plausible expiry. Nothing in this module implements `RefreshOauth`,
    // so a credential that ever reported itself due would have no way to
    // stop being due.
    for now_ms in [0, 1, super::super::now_ms(), u64::MAX - REFRESH_SKEW_MS, u64::MAX] {
        assert!(
            !credential.needs_refresh(now_ms, REFRESH_SKEW_MS),
            "a Copilot credential must never be due, and was at {now_ms}"
        );
    }
    assert!(
        !credential.needs_refresh(u64::MAX, 0),
        "and at no margin at all either, for the same reason"
    );
}
