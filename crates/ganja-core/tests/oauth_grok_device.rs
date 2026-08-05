//! A headless xAI login, from the code on the screen to the renewal that
//! follows it.
//!
//! What this proves that the unit tests cannot: the login flow and the store
//! are genuinely separate. A login that was abandoned, and a login that
//! *succeeded*, both leave `auth.json` untouched — storing is the caller's
//! step, so there is no path by which a half-finished login writes a
//! credential. Then the other half: once the caller does store it, the file it
//! lands in is the one an opencode install reads, under the key upstream
//! writes, and the renewal that follows goes through the whole public
//! chain — `Refresher` holding it to one, `grok::Refresh` spending the token,
//! the rotated pair coming back to disk.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`, and `cargo
//! test` runs the tests inside a binary on parallel threads.
//!
//! Nothing here mocks the HTTP client. The requests asserted on are the
//! requests that were actually built and sent.

use std::{
    env, fs,
    sync::{Arc, Mutex},
};

use ganja_core::auth::{self, AuthErrorKind, OauthCredential, REFRESH_SKEW_MS, Refresher, grok};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

/// The refresh token the first login yields. It must reach the file and
/// nothing else.
const FIRST_REFRESH: &str = "rt-grok-first-AAAA";

/// What the renewal rotates it to.
const SECOND_REFRESH: &str = "rt-grok-second-BBBB";

/// A loopback endpoint answering canned bodies in order.
struct Endpoint {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// Every request body that arrived, in order.
    fn bodies(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn count(&self) -> usize {
        self.bodies().len()
    }
}

/// Serves `replies` as `(status, body)`, one per connection, then stops.
async fn serve(replies: Vec<(u16, String)>) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));

    let server = tokio::spawn({
        let requests = Arc::clone(&requests);
        async move {
            for (status, body) in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };

                let request = read(&mut socket).await;
                requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request);

                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        }
    });

    Endpoint {
        url,
        requests,
        _server: server,
    }
}

/// Reads a request and hands back its body.
async fn read(socket: &mut TcpStream) -> String {
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

    String::from_utf8_lossy(&body).into_owned()
}

/// A clock the test drives, so the poll's wait costs the suite nothing.
///
/// `auth::device::Clock` is public for exactly this: a caller outside the
/// crate can supply time. Waiting the real eight seconds here would buy no
/// coverage — the cadence itself is pinned by the unit tests, which assert the
/// waits that were asked for — and would cost every future run of the suite.
struct Instant {
    now_ms: Mutex<u64>,
}

impl Instant {
    fn at(now_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            now_ms: Mutex::new(now_ms),
        })
    }
}

#[async_trait::async_trait]
impl auth::device::Clock for Instant {
    fn now_ms(&self) -> u64 {
        *self
            .now_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn sleep(&self, duration: std::time::Duration) {
        // Time passes exactly as far as it was asked to, so the deadline the
        // flow computes still means what it means.
        let mut now = self
            .now_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
async fn a_headless_login_stores_nothing_until_it_is_told_to_and_then_renews_itself() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
    }

    let endpoint = serve(vec![
        // The login that gets abandoned.
        (
            200,
            r#"{"device_code":"dev-abandoned","user_code":"AAAA-1111",
                "verification_uri":"https://accounts.x.ai/device","interval":5,
                "expires_in":600}"#
                .to_owned(),
        ),
        // The login that completes.
        (
            200,
            r#"{"device_code":"dev-real","user_code":"BBBB-2222",
                "verification_uri":"https://accounts.x.ai/device","interval":5,
                "expires_in":600}"#
                .to_owned(),
        ),
        // xAI's spelling of "not yet": the RFC's 400 with the code in the body.
        (400, r#"{"error":"authorization_pending"}"#.to_owned()),
        (
            200,
            format!(
                r#"{{"access_token":"at-grok-first","refresh_token":"{FIRST_REFRESH}",
                     "expires_in":3600,"token_type":"Bearer"}}"#
            ),
        ),
        // The renewal, rotating the refresh token as xAI does.
        (
            200,
            format!(
                r#"{{"access_token":"at-grok-second","refresh_token":"{SECOND_REFRESH}",
                     "expires_in":3600}}"#
            ),
        ),
    ])
    .await;
    let device_url = format!("{}/device", endpoint.url);
    let token_url = format!("{}/token", endpoint.url);

    // A login that is started and then abandoned. The code was issued — the
    // provider has seen it — and still nothing is written, because writing is
    // not this flow's to do.
    let abandoned = CancellationToken::new();
    let flow = grok::device_flow_at(device_url.as_str(), token_url.as_str())
        .expect("a client builds")
        .with_clock(Instant::at(auth::now_ms()));
    let started = flow.start(&abandoned).await.expect("the code is issued");
    assert_eq!(started.user_code, "AAAA-1111");
    abandoned.cancel();
    let failure = flow
        .poll(&started, &abandoned)
        .await
        .expect_err("a cancelled login is not a login");
    assert!(
        matches!(failure, ganja_core::auth::device::DeviceError::Cancelled),
        "expected a cancellation, got {failure:?}"
    );
    assert_eq!(endpoint.count(), 1, "the abandoned poll asked for nothing");
    assert!(
        stored().is_none(),
        "a cancelled login must leave no credential file at all"
    );

    // A login that completes.
    let cancel = CancellationToken::new();
    let flow = grok::device_flow_at(device_url.as_str(), token_url.as_str())
        .expect("a client builds")
        .with_clock(Instant::at(auth::now_ms()));
    let started = flow.start(&cancel).await.expect("the code is issued");
    assert_eq!(started.user_code, "BBBB-2222");
    assert_eq!(
        started.browser_url(),
        "https://accounts.x.ai/device",
        "with no pre-filled page on offer, the plain one is what to open"
    );
    let credential = grok::credential_from(&flow.poll(&started, &cancel).await.expect("it lands"));

    assert_eq!(credential.refresh.expose_secret(), FIRST_REFRESH);
    assert_eq!(credential.access.expose_secret(), "at-grok-first");
    assert!(
        !credential.needs_refresh(auth::now_ms(), REFRESH_SKEW_MS),
        "a token good for an hour is not due"
    );
    // The claim this file exists for: the flow succeeded and still wrote
    // nothing. There is no store in its reach to write to.
    assert!(
        stored().is_none(),
        "a completed login must not store anything either - the caller decides"
    );

    // Now the caller stores it, and it lands where an opencode install looks.
    auth::set_oauth(grok::PROVIDER_ID, &credential).expect("the credential stores");
    let file = stored().expect("the store exists now");
    assert!(
        file.get("xai").is_some(),
        "upstream's key is what a shared auth.json is read by: {file}"
    );
    assert!(
        file.get("grok").is_none(),
        "ganja's own name for it must not reach the file: {file}"
    );
    assert_eq!(
        auth::oauth_for(grok::PROVIDER_ID)
            .expect("the store reads")
            .expect("the credential is there")
            .refresh
            .expose_secret(),
        FIRST_REFRESH,
        "and it reads back under the name ganja calls it"
    );

    // A credential that has run out, renewed through the whole public chain.
    let spent = OauthCredential::new(
        SecretString::from(FIRST_REFRESH),
        SecretString::from("at-grok-first"),
        auth::now_ms(),
    );
    auth::set_oauth(grok::PROVIDER_ID, &spent).expect("the spent credential stores");

    let refresher = Refresher::new();
    let renewed = refresher
        .usable(
            grok::PROVIDER_ID,
            Arc::new(grok::Refresh::at(token_url.as_str()).expect("a client builds")),
        )
        .await
        .expect("the endpoint renewed it");

    assert_eq!(renewed.access.expose_secret(), "at-grok-second");
    assert_eq!(
        renewed.refresh.expose_secret(),
        SECOND_REFRESH,
        "xAI rotates, and the spent token must not survive the renewal"
    );
    assert_eq!(
        auth::oauth_for(grok::PROVIDER_ID)
            .expect("the store reads")
            .expect("the credential is there")
            .refresh
            .expose_secret(),
        SECOND_REFRESH,
        "the rotated pair has to reach disk, or the next process presents a spent token"
    );

    // What actually went over the wire: the device grant, then the refresh
    // grant, each with the token it is supposed to present.
    let bodies = endpoint.bodies();
    assert_eq!(bodies.len(), 5, "five requests, and no retries: {bodies:?}");
    assert!(
        bodies[3].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
        "the poll presents the device grant: {}",
        bodies[3]
    );
    assert!(
        bodies[4].contains("grant_type=refresh_token")
            && bodies[4].contains(&format!("refresh_token={FIRST_REFRESH}")),
        "the renewal presents the token that was stored: {}",
        bodies[4]
    );

    // And a provider that has no OAuth credential says so, rather than
    // reporting the one next door.
    let missing = refresher
        .usable(
            "anthropic",
            Arc::new(grok::Refresh::at(token_url.as_str()).expect("a client builds")),
        )
        .await
        .expect_err("anthropic has nothing stored");
    assert_eq!(missing.kind(), AuthErrorKind::NotOauth);
    assert_eq!(
        endpoint.count(),
        5,
        "a provider with nothing stored must not cost a request"
    );
}
