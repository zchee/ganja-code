//! A ChatGPT login against a real socket, from the authorize URL to the store.
//!
//! What this proves that the unit tests cannot: the request that is actually
//! built and sent, the poll cadence that is actually waited out, and what the
//! credential store actually holds afterwards. The issuer is a loopback server
//! serving canned answers, so every byte on the wire is one the login really
//! produced — `Login::with_issuer` exists for exactly this, and upstream
//! carries the same override for the same reason (`codex.ts:101-105`).
//!
//! **One test, one binary, on purpose.** It mutates `XDG_DATA_HOME`, and
//! `cargo test` runs the tests inside a binary on parallel threads — where the
//! other tests here would be building HTTP clients, and `reqwest`'s proxy
//! resolution reads the environment. The story is therefore told in phases,
//! each a named function, so a failure still says which sentence broke.
//!
//! The pure halves — the authorize URL's parameters, the account-id claim
//! order, the refusal classification — are unit tests in `auth::openai`, and
//! the callback listener's own behaviour is unit-tested in `auth::loopback`.
//! Neither needs a process-wide anything.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs};

use ganja_core::auth::openai::{DEVICE_DEADLINE, Login, PROVIDER_ID};
use ganja_core::auth::{self, AuthErrorKind, OauthCredential, RefreshOauth as _, pkce};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Longer than any phase here needs, so nothing is decided by a clock.
const AMPLE: Duration = Duration::from_secs(60);

/// The API key a ChatGPT login is about to cost somebody.
const API_KEY: &str = "sk-openai-must-not-survive-0001";

/// Where the token endpoint lives.
const TOKEN: &str = "/oauth/token";

/// Where a device login asks for a code.
const USERCODE: &str = "/api/accounts/deviceauth/usercode";

/// Where a device login waits for it to be entered.
const DEVICE_TOKEN: &str = "/api/accounts/deviceauth/token";

/// One request the issuer was sent.
#[derive(Clone, Debug)]
struct Seen {
    /// What it asked for.
    path: String,
    /// What it said, verbatim, so a form body can be read back pair by pair.
    body: String,
    /// What it called itself.
    user_agent: Option<String>,
    /// What it said its body was.
    content_type: Option<String>,
}

impl Seen {
    /// One pair out of a form-encoded body.
    fn field(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }
}

/// One canned answer.
#[derive(Clone)]
struct Reply {
    /// The status to answer with.
    status: u16,
    /// The JSON body.
    body: String,
}

impl Reply {
    /// A `200` carrying `body`.
    fn ok(body: serde_json::Value) -> Self {
        Self { status: 200, body: body.to_string() }
    }

    /// A refusal with no body worth reading.
    fn status(status: u16) -> Self {
        Self { status, body: "{}".to_owned() }
    }

    /// A refusal naming its own OAuth error code.
    fn refused(status: u16, code: &str) -> Self {
        Self { status, body: serde_json::json!({ "error": code }).to_string() }
    }
}

/// What the issuer answers, path by path.
///
/// The last answer for a path repeats once its queue is down to one, so "403
/// until something else happens" is a single entry rather than a guess about
/// how many polls will fit inside a deadline.
#[derive(Default)]
struct Script(HashMap<String, Vec<Reply>>);

impl Script {
    /// Answers `path` with `replies`, in order.
    fn on(mut self, path: &str, replies: Vec<Reply>) -> Self {
        self.0.insert(path.to_owned(), replies);

        self
    }
}

/// A stand-in for `auth.openai.com`.
struct Issuer {
    /// What to point a [`Login`] at.
    base: String,
    /// Every request it was sent, in order.
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Kept so the server outlives the phase talking to it.
    _server: tokio::task::JoinHandle<()>,
}

impl Issuer {
    /// A login pointed at this issuer.
    fn login(&self) -> Login {
        Login::with_issuer(&self.base).expect("loopback is allowed to carry a login")
    }

    /// Every request sent to `path`.
    fn sent_to(&self, path: &str) -> Vec<Seen> {
        self.seen
            .lock()
            .expect("no phase panicked while holding this")
            .iter()
            .filter(|seen| seen.path == path)
            .cloned()
            .collect()
    }
}

/// Stands up an issuer serving `script`.
async fn issuer(script: Script) -> Issuer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let base = format!("http://{}", listener.local_addr().expect("a bound socket has an address"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);

    let server = tokio::spawn(async move {
        let mut queues: HashMap<String, VecDeque<Reply>> =
            script.0.into_iter().map(|(path, replies)| (path, replies.into())).collect();

        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Some(request) = read(&mut socket).await else {
                continue;
            };
            let reply = match queues.get_mut(&request.path) {
                Some(queue) if queue.len() > 1 => queue.pop_front().unwrap_or_else(unreachable),
                Some(queue) => queue.front().cloned().unwrap_or_else(|| Reply::status(404)),
                None => Reply::status(404),
            };
            recorded.lock().expect("no phase panicked while holding this").push(request);
            write(&mut socket, &reply).await;
        }
    });

    Issuer { base, seen, _server: server }
}

/// A queue that was just checked to be non-empty cannot be empty.
fn unreachable() -> Reply {
    Reply::status(500)
}

/// Reads one request, head and body.
async fn read(socket: &mut TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];

    let end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };

    let head = String::from_utf8(buffer[..end].to_vec()).ok()?;
    let header = |name: &str| {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    };
    let length: usize = header("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);

    let mut body = buffer[end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Some(Seen {
        path: head.lines().next()?.split(' ').nth(1)?.split('?').next()?.to_owned(),
        body: String::from_utf8(body).ok()?,
        user_agent: header("user-agent"),
        content_type: header("content-type"),
    })
}

/// Answers one request.
async fn write(socket: &mut TcpStream, reply: &Reply) {
    let response = format!(
        "HTTP/1.1 {status} Status\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {length}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        status = reply.status,
        length = reply.body.len(),
        body = reply.body,
    );

    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
    let _ = socket.shutdown().await;
}

/// Delivers the provider's redirect to a waiting browser login.
async fn redirect(port: u16, query: &str) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.expect("the login is listening");
    socket
        .write_all(
            format!(
                "GET /auth/callback?{query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("the redirect is written");

    let mut answered = String::new();
    socket.read_to_string(&mut answered).await.expect("the page comes back");
}

/// One parameter out of an authorize URL.
fn parameter(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .expect("the authorize URL is a URL")
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("the authorize URL publishes {name}"))
}

/// What a token endpoint answers a completed login with.
fn tokens(refresh: &str, access: &str) -> serde_json::Value {
    serde_json::json!({
        "id_token": "eyJhbGciOiJSUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0LTQyIn0.nope",
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 3600,
    })
}

#[tokio::test]
async fn a_chatgpt_login_exchanges_polls_renews_and_stores_exactly_what_it_should() {
    let home = tempfile::tempdir().expect("a temporary directory");

    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        // A stored credential is what has to answer here; an exported key would
        // win the lookup and the file would never be read.
        env::remove_var("OPENAI_API_KEY");
    }

    the_code_exchange_presents_the_verifier_the_authorize_url_published().await;
    a_403_and_a_404_from_the_device_endpoint_both_mean_keep_waiting().await;
    the_verifier_the_device_endpoint_mints_is_the_one_the_exchange_presents().await;
    a_device_login_nobody_completes_ends_at_its_deadline().await;
    a_device_login_ends_promptly_when_it_is_cancelled().await;
    a_renewal_that_rotates_the_refresh_token_yields_the_new_one().await;
    a_renewal_that_returns_no_new_token_keeps_the_old_one().await;
    a_refused_renewal_and_an_unreachable_one_are_different_situations().await;
    a_login_that_never_completed_stores_nothing().await;
    a_chatgpt_login_replaces_a_stored_openai_api_key().await;
}

/// The whole browser path: the URL published, the redirect answered, and the
/// exchange carrying the verifier that challenge was computed over.
async fn the_code_exchange_presents_the_verifier_the_authorize_url_published() {
    let issuer = issuer(Script::default().on(TOKEN, vec![Reply::ok(tokens("rt-1", "at-1"))])).await;
    let browser = issuer.login().browser_on(0).await.expect("loopback is bindable");

    let published = parameter(browser.url(), "code_challenge");
    let state = parameter(browser.url(), "state");
    let redirect_uri = parameter(browser.url(), "redirect_uri");
    let port = browser.port();

    let waiting = tokio::spawn(async move { browser.wait(AMPLE, &CancellationToken::new()).await });
    redirect(port, &format!("code=the-code&state={state}")).await;
    let credential = waiting.await.expect("the wait finished").expect("the exchange succeeded");

    let sent = issuer.sent_to(TOKEN);
    assert_eq!(sent.len(), 1, "one code buys one exchange");
    let exchange = &sent[0];

    assert_eq!(
        exchange.content_type.as_deref(),
        Some("application/x-www-form-urlencoded"),
        "the token endpoint takes a form, not JSON"
    );
    assert_eq!(exchange.field("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(exchange.field("code").as_deref(), Some("the-code"));
    assert_eq!(exchange.field("redirect_uri").as_deref(), Some(redirect_uri.as_str()));
    assert_eq!(exchange.field("client_id").as_deref(), Some("app_EMoamEEZ73f0CkXaXp7hrann"));

    let presented = exchange.field("code_verifier").expect("the exchange presents a verifier");
    assert_eq!(
        pkce::challenge_for(&presented),
        published,
        "the issuer recomputes the challenge over this exact string; if they \
         disagree it refuses the exchange after the person has already finished \
         in the browser"
    );

    assert!(
        exchange.user_agent.as_deref().is_some_and(|agent| agent.starts_with("ganja-code/")),
        "the token exchange says what this build is, not what it borrowed a \
         client registration from: {:?}",
        exchange.user_agent
    );

    assert_eq!(credential.access.expose_secret(), "at-1");
    assert_eq!(credential.refresh.expose_secret(), "rt-1");
    assert_eq!(
        credential.account_id.as_deref(),
        Some("acct-42"),
        "read out of the id_token, whose signature is the word `nope`"
    );
    assert!(credential.expires > auth::now_ms(), "an hour from now");
}

/// The pending signal on this flow is a status, not an error body.
async fn a_403_and_a_404_from_the_device_endpoint_both_mean_keep_waiting() {
    let issuer = issuer(
        Script::default()
            .on(
                USERCODE,
                vec![Reply::ok(serde_json::json!({
                    "device_auth_id": "dev-1", "user_code": "ABCD-EFGH", "interval": "1",
                }))],
            )
            .on(
                DEVICE_TOKEN,
                vec![
                    Reply::status(403),
                    Reply::status(404),
                    Reply::ok(serde_json::json!({
                        "authorization_code": "dev-code", "code_verifier": "dev-verifier",
                    })),
                ],
            )
            .on(TOKEN, vec![Reply::ok(tokens("rt-dev", "at-dev"))]),
    )
    .await;

    let device = issuer.login().device().await.expect("a code was issued");
    assert_eq!(device.user_code(), "ABCD-EFGH");
    assert!(device.url().ends_with("/codex/device"), "{}", device.url());

    let credential =
        device.wait(AMPLE, &CancellationToken::new()).await.expect("the third poll succeeded");

    assert_eq!(
        issuer.sent_to(DEVICE_TOKEN).len(),
        3,
        "a 403 and a 404 each cost one more poll rather than the login"
    );
    assert_eq!(credential.access.expose_secret(), "at-dev");
}

/// On this path the *server* mints the PKCE secret.
async fn the_verifier_the_device_endpoint_mints_is_the_one_the_exchange_presents() {
    let issuer = issuer(
        Script::default()
            .on(
                USERCODE,
                vec![Reply::ok(serde_json::json!({
                    "device_auth_id": "dev-1", "user_code": "ABCD-EFGH", "interval": "1",
                }))],
            )
            .on(
                DEVICE_TOKEN,
                vec![Reply::ok(serde_json::json!({
                    "authorization_code": "dev-code",
                    "code_verifier": "the-servers-own-verifier",
                }))],
            )
            .on(TOKEN, vec![Reply::ok(tokens("rt-dev", "at-dev"))]),
    )
    .await;

    issuer
        .login()
        .device()
        .await
        .expect("a code was issued")
        .wait(AMPLE, &CancellationToken::new())
        .await
        .expect("the first poll succeeded");

    let exchange = &issuer.sent_to(TOKEN)[0];
    assert_eq!(
        exchange.field("code_verifier").as_deref(),
        Some("the-servers-own-verifier"),
        "a verifier generated here would simply be the wrong one"
    );
    assert_eq!(
        exchange.field("redirect_uri").as_deref(),
        Some(format!("{}/deviceauth/callback", issuer.base).as_str()),
        "the device path's redirect is the issuer's own, not a loopback one"
    );
    assert_eq!(exchange.field("code").as_deref(), Some("dev-code"));
}

/// Upstream's loop has no deadline; this one does.
async fn a_device_login_nobody_completes_ends_at_its_deadline() {
    let issuer = issuer(
        Script::default()
            .on(
                USERCODE,
                vec![Reply::ok(serde_json::json!({
                    "device_auth_id": "dev-1", "user_code": "ABCD-EFGH", "interval": "1",
                }))],
            )
            // Forever, because the queue's last answer repeats.
            .on(DEVICE_TOKEN, vec![Reply::status(403)]),
    )
    .await;

    let ended = issuer
        .login()
        .device()
        .await
        .expect("a code was issued")
        .wait(Duration::from_millis(250), &CancellationToken::new())
        .await
        .expect_err("nobody ever entered the code");

    assert!(ended.to_string().contains("was not completed within"), "{ended}");
    assert!(
        DEVICE_DEADLINE > Duration::from_millis(250),
        "the production bound is the one in the constant, not this"
    );
}

/// The deadline is minutes; a keystroke is now.
async fn a_device_login_ends_promptly_when_it_is_cancelled() {
    let issuer = issuer(
        Script::default()
            .on(
                USERCODE,
                vec![Reply::ok(serde_json::json!({
                    "device_auth_id": "dev-1", "user_code": "ABCD-EFGH", "interval": "1",
                }))],
            )
            .on(DEVICE_TOKEN, vec![Reply::status(403)]),
    )
    .await;

    let device = issuer.login().device().await.expect("a code was issued");
    let cancel = CancellationToken::new();
    let waiting = {
        let cancel = cancel.clone();
        tokio::spawn(async move { device.wait(AMPLE, &cancel).await })
    };
    cancel.cancel();

    let ended = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("cancelling is not something to wait out")
        .expect("the wait finished")
        .expect_err("cancelling yields no credential");

    assert!(ended.to_string().contains("cancelled"), "{ended}");
}

/// ChatGPT's refresh token rotates, which is what `Refresher` exists for.
async fn a_renewal_that_rotates_the_refresh_token_yields_the_new_one() {
    let issuer =
        issuer(Script::default().on(TOKEN, vec![Reply::ok(tokens("rt-rotated", "at-2"))])).await;
    let stale = spent();

    let renewed = issuer.login().refresh(PROVIDER_ID, &stale).await.expect("the issuer renewed it");

    let sent = &issuer.sent_to(TOKEN)[0];
    assert_eq!(sent.field("grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(sent.field("refresh_token").as_deref(), Some("rt-old"));

    assert_eq!(renewed.refresh.expose_secret(), "rt-rotated");
    assert_eq!(renewed.access.expose_secret(), "at-2");
    assert_eq!(renewed.account_id.as_deref(), Some("acct-42"), "carried across the renewal");
}

/// An endpoint that stayed silent about the refresh token has not revoked it.
async fn a_renewal_that_returns_no_new_token_keeps_the_old_one() {
    let issuer = issuer(Script::default().on(
        TOKEN,
        vec![Reply::ok(serde_json::json!({
            "access_token": "at-2", "expires_in": 3600,
        }))],
    ))
    .await;

    let renewed =
        issuer.login().refresh(PROVIDER_ID, &spent()).await.expect("the issuer renewed it");

    assert_eq!(
        renewed.refresh.expose_secret(),
        "rt-old",
        "dropping it would leave nothing to renew with next time"
    );
    assert_eq!(renewed.access.expose_secret(), "at-2");
}

/// One says log in again; the other says try again. Never the reverse.
async fn a_refused_renewal_and_an_unreachable_one_are_different_situations() {
    let refusing =
        issuer(Script::default().on(TOKEN, vec![Reply::refused(401, "invalid_grant")])).await;
    let refused =
        refusing.login().refresh(PROVIDER_ID, &spent()).await.expect_err("the grant is gone");

    assert_eq!(refused.kind(), AuthErrorKind::ReauthRequired);
    assert!(refused.to_string().contains("invalid_grant"), "{refused}");

    let limited =
        issuer(Script::default().on(TOKEN, vec![Reply::refused(429, "rate_limit_exceeded")]))
            .await
            .login()
            .refresh(PROVIDER_ID, &spent())
            .await
            .expect_err("not now");
    assert_eq!(
        limited.kind(),
        AuthErrorKind::RefreshUnavailable,
        "a queue is not a dead grant: {limited}"
    );

    // Bound, read the port, drop the listener — nothing is there to answer.
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let base = format!("http://{}", closed.local_addr().expect("a bound socket has an address"));
    drop(closed);

    let unreachable = Login::with_issuer(&base)
        .expect("loopback is allowed")
        .refresh(PROVIDER_ID, &spent())
        .await
        .expect_err("nothing answered");
    assert_eq!(
        unreachable.kind(),
        AuthErrorKind::RefreshUnavailable,
        "telling somebody whose network dropped to open a browser is the defect \
         this class exists to prevent: {unreachable}"
    );

    for message in [refused.to_string(), unreachable.to_string()] {
        assert!(!message.contains("rt-old"), "a token reached a message: {message}");
        assert!(!message.contains("at-old"), "a token reached a message: {message}");
    }
}

/// Nothing in this lane can store anything, so nothing does.
async fn a_login_that_never_completed_stores_nothing() {
    let before = auth::list_providers().expect("an absent store lists nothing");

    let issuer = issuer(Script::default()).await;
    let browser = issuer.login().browser_on(0).await.expect("loopback is bindable");
    let port = browser.port();

    let cancel = CancellationToken::new();
    let waiting = {
        let cancel = cancel.clone();
        tokio::spawn(async move { browser.wait(AMPLE, &cancel).await })
    };
    cancel.cancel();
    waiting.await.expect("the wait finished").expect_err("cancelling yields no credential");

    // And a forged callback, on a second login, likewise.
    let forged = issuer.login().browser_on(0).await.expect("loopback is bindable");
    let forged_port = forged.port();
    let waiting = tokio::spawn(async move { forged.wait(AMPLE, &CancellationToken::new()).await });
    redirect(forged_port, "code=stolen&state=not-the-one").await;
    waiting.await.expect("the wait finished").expect_err("a callback from elsewhere buys nothing");

    assert_eq!(auth::list_providers().expect("an absent store still lists nothing"), before,);
    assert!(
        !auth::store_path().expect("a path resolves").exists(),
        "a login that failed wrote a credential file"
    );
    assert_ne!(
        port, forged_port,
        "each login bound its own socket, so neither could have answered for \
         the other"
    );
}

/// The hazard, on the record: one key, two kinds of credential.
async fn a_chatgpt_login_replaces_a_stored_openai_api_key() {
    auth::set_credential(PROVIDER_ID, API_KEY).expect("a key is storable");
    assert!(
        auth::credential_for(PROVIDER_ID)
            .expect("the store reads")
            .is_some_and(|stored| stored.api_key.expose_secret() == API_KEY)
    );

    let issuer = issuer(Script::default().on(TOKEN, vec![Reply::ok(tokens("rt-1", "at-1"))])).await;
    let browser = issuer.login().browser_on(0).await.expect("loopback is bindable");
    let state = parameter(browser.url(), "state");
    let port = browser.port();
    let waiting = tokio::spawn(async move { browser.wait(AMPLE, &CancellationToken::new()).await });
    redirect(port, &format!("code=the-code&state={state}")).await;
    let credential = waiting.await.expect("the wait finished").expect("the exchange succeeded");

    // The one line a caller writes, and the one line this whole hazard is.
    auth::set_oauth(PROVIDER_ID, &credential).expect("a credential is storable");

    assert!(
        auth::credential_for(PROVIDER_ID).expect("the store reads").is_none(),
        "the API key is gone - upstream's behaviour at this key too, and \
         warning somebody first belongs to whatever runs the login"
    );
    assert_eq!(
        auth::oauth_for(PROVIDER_ID)
            .expect("the store reads")
            .expect("the login is stored")
            .access
            .expose_secret(),
        "at-1"
    );
    assert!(
        !fs::read_to_string(auth::store_path().expect("a path resolves"))
            .expect("the file is there")
            .contains(API_KEY),
        "the key is not merely shadowed, it is gone"
    );

    // And back the other way, which is the half somebody logging in expects.
    auth::set_credential(PROVIDER_ID, API_KEY).expect("a key is storable");
    assert!(
        auth::oauth_for(PROVIDER_ID).expect("the store reads").is_none(),
        "storing an API key takes the ChatGPT login with it"
    );
}

/// A credential due for renewal, as one would be read off disk.
fn spent() -> OauthCredential {
    let mut credential =
        OauthCredential::new(SecretString::from("rt-old"), SecretString::from("at-old"), 1);
    credential.account_id = Some("acct-42".to_owned());

    credential
}
