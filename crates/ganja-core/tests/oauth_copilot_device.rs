//! A GitHub Copilot login against an enterprise deployment, and the record it
//! leaves.
//!
//! What this proves that the unit tests cannot: the credential survives the
//! round trip through the real file in the shape upstream wrote it — one token
//! in both fields, `expires: 0`, and the enterprise deployment beside it — and
//! that `expires: 0` still reads as **never** after it has been through
//! `serde`. That last one is the trap this provider carries: there is no
//! refresh endpoint anywhere in the Copilot plugin, so a credential that ever
//! reported itself due would be due forever.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`, and `cargo
//! test` runs the tests inside a binary on parallel threads.
//!
//! Nothing here mocks the HTTP client. The JSON body asserted on is the body
//! that was actually built and sent — and it being JSON at all is a divergence
//! from every other device flow in this build.

use std::sync::{Arc, Mutex};
use std::{env, fs};

use ganja_core::auth::{self, CredentialKind, REFRESH_SKEW_MS, copilot};
use secrecy::ExposeSecret as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// The GitHub token the login yields. It is the whole credential.
const TOKEN: &str = "gho_copilot-integration-DDDD";

/// A loopback endpoint answering canned bodies in order.
struct Endpoint {
    url: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// Every request that arrived, as `(head, body)`.
    fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// Serves `bodies` with a `200` each, one per connection, then stops.
///
/// Every reply is a 200 on purpose: GitHub answers a device poll that is not
/// finished yet with `200 {"error":"authorization_pending"}` rather than with
/// the RFC's 400, and a loop that read the status before the body would take
/// that for success — or, read the other way round, would end xAI's flow on
/// its first poll. Serving GitHub's spelling here is what holds that.
async fn serve(bodies: Vec<String>) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let url = format!("http://{}", listener.local_addr().expect("a bound socket has an address"));
    let requests = Arc::new(Mutex::new(Vec::new()));

    let server = tokio::spawn({
        let requests = Arc::clone(&requests);
        async move {
            for body in bodies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };

                let request = read(&mut socket).await;
                requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(request);

                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        }
    });

    Endpoint { url, requests, _server: server }
}

/// Reads a request as `(head, body)`.
async fn read(socket: &mut TcpStream) -> (String, String) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => break,
            Ok(_) => head.push(byte[0]),
        }
    }

    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(": ")?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 {
        let _ = socket.read_exact(&mut body).await;
    }

    (head, String::from_utf8_lossy(&body).into_owned())
}

/// A clock the test drives, so the poll's wait costs the suite nothing.
///
/// `auth::device::Clock` is public for exactly this: a caller outside the
/// crate can supply time. Waiting the real eight seconds here would buy no
/// coverage — the cadence is pinned by the unit tests, which assert the waits
/// that were asked for — and would cost every future run of the suite.
struct Instant {
    now_ms: Mutex<u64>,
}

impl Instant {
    fn at(now_ms: u64) -> Arc<Self> {
        Arc::new(Self { now_ms: Mutex::new(now_ms) })
    }
}

#[async_trait::async_trait]
impl auth::device::Clock for Instant {
    fn now_ms(&self) -> u64 {
        *self.now_ms.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn sleep(&self, duration: std::time::Duration) {
        // Time passes exactly as far as it was asked to, so the deadline the
        // flow computes still means what it means.
        let mut now = self.now_ms.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *now = now.saturating_add(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    }
}

/// The credential file as JSON, or `None` when there is not one.
fn stored() -> Option<serde_json::Value> {
    let path = auth::store_path().expect("the store has a path");

    fs::read_to_string(path)
        .ok()
        .map(|text| serde_json::from_str(&text).expect("the store is JSON"))
}

#[tokio::test]
async fn an_enterprise_copilot_login_stores_one_token_that_never_needs_renewing() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
    }

    let endpoint = serve(vec![
        r#"{"device_code":"dev-copilot","user_code":"CDCD-3434",
            "verification_uri":"https://company.ghe.com/login/device","interval":5}"#
            .to_owned(),
        // GitHub's spelling of "not yet": a 200 carrying the error.
        r#"{"error":"authorization_pending"}"#.to_owned(),
        format!(r#"{{"access_token":"{TOKEN}","token_type":"bearer","scope":"read:user"}}"#),
    ])
    .await;

    // Every spelling a person might have typed at the prompt lands on the same
    // deployment; the pasted-address one is the spelling used here.
    let deployment = copilot::Deployment::enterprise("https://company.ghe.com/");
    assert_eq!(deployment.domain(), "company.ghe.com");
    assert_eq!(
        copilot::api_base_for(deployment.domain()),
        "https://copilot-api.company.ghe.com",
        "an enterprise deployment's requests do not go to api.githubcopilot.com"
    );

    let cancel = CancellationToken::new();
    let flow = copilot::device_flow_at(
        format!("{}/login/device/code", endpoint.url),
        format!("{}/login/oauth/access_token", endpoint.url),
    )
    .expect("a client builds")
    .with_clock(Instant::at(auth::now_ms()));

    let started = flow.start(&cancel).await.expect("the code is issued");
    assert_eq!(started.user_code, "CDCD-3434");
    // GitHub sends no `expires_in`, so the only bound this login has is the
    // one ganja supplies. Upstream's loop has none at all.
    assert!(
        started.deadline_ms() > auth::now_ms(),
        "a login with no stated lifetime still has to be bounded"
    );

    let credential = copilot::credential_from(
        &flow.poll(&started, &cancel).await.expect("the login lands"),
        &deployment,
    );

    // Nothing is stored until the caller stores it, here as elsewhere.
    assert!(stored().is_none(), "a completed login must not write a credential of its own accord");

    auth::set_oauth(copilot::PROVIDER_ID, &credential).expect("the credential stores");

    // The record upstream writes, field for field (`copilot.ts:286-305`).
    let entry = stored().expect("the store exists now");
    let entry = entry.get("github-copilot").expect("stored under upstream's own name for it");
    assert_eq!(entry.get("type").and_then(serde_json::Value::as_str), Some("oauth"));
    assert_eq!(entry.get("access").and_then(serde_json::Value::as_str), Some(TOKEN));
    assert_eq!(
        entry.get("refresh").and_then(serde_json::Value::as_str),
        Some(TOKEN),
        "there is one token, and every request reads it out of `refresh`"
    );
    assert_eq!(
        entry.get("expires").and_then(serde_json::Value::as_u64),
        Some(0),
        "zero is upstream's `never`, and it has to reach the file as zero"
    );
    assert_eq!(
        entry.get("enterpriseUrl").and_then(serde_json::Value::as_str),
        Some("company.ghe.com"),
        "the deployment has to travel with the token, or requests go to the wrong host"
    );

    // Read back through the public path, and still never due — at any clock a
    // caller could possibly ask with. Nothing implements `RefreshOauth` for
    // this provider, so "due" would be a state it could never leave.
    let read_back = auth::oauth_for(copilot::PROVIDER_ID)
        .expect("the store reads")
        .expect("the credential is there");
    assert_eq!(read_back.access.expose_secret(), TOKEN);
    assert_eq!(read_back.enterprise_url.as_deref(), Some("company.ghe.com"));
    assert_eq!(read_back.expires, 0);
    for now_ms in [0, auth::now_ms(), u64::MAX - REFRESH_SKEW_MS, u64::MAX] {
        assert!(
            !read_back.needs_refresh(now_ms, REFRESH_SKEW_MS),
            "a stored Copilot credential must never be due, and was at {now_ms}"
        );
        assert!(
            !read_back.needs_refresh(now_ms, 0),
            "and must not be due at no margin either, for the same reason"
        );
    }
    assert!(
        auth::list_providers()
            .expect("the store lists")
            .iter()
            .any(|listed| listed.provider_id == "github-copilot"
                && listed.kind == CredentialKind::Oauth),
        "and it lists as the kind of credential it is"
    );

    // What went over the wire: JSON, not a form, with upstream's headers.
    let requests = endpoint.requests();
    assert_eq!(requests.len(), 3, "one start and two polls");
    for (head, _) in &requests {
        assert!(
            head.to_lowercase().contains("content-type: application/json"),
            "GitHub's device endpoints take JSON, not a form: {head}"
        );
        assert!(
            head.to_lowercase().contains("user-agent: opencode/"),
            "the client id is upstream's registered application, and the agent is the \
             product name the live spike measured against it: {head}"
        );
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&requests[0].1).expect("the body is JSON"),
        serde_json::json!({"client_id": "Ov23li8tweQw6odWQebz", "scope": "read:user"}),
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&requests[2].1).expect("the body is JSON"),
        serde_json::json!({
            "client_id": "Ov23li8tweQw6odWQebz",
            "device_code": "dev-copilot",
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        }),
    );
    assert!(
        !requests.iter().any(|(head, body)| head.contains(TOKEN) || body.contains(TOKEN)),
        "the token is what comes back, never what goes out"
    );
}
