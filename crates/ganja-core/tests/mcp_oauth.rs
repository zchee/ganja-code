//! F5's OAuth half, end to end: discovery → PKCE → token, stored under
//! `mcp:<name>`, sent as the bearer on a dial, refreshed and re-dialled when
//! the server answers 401.
//!
//! One test, one file: the credential store this exercises resolves through
//! `XDG_DATA_HOME`, which is process-wide — the same house rule
//! `crates/ganja-core/tests/config_hooks_global.rs` states for
//! `GANJA_CONFIG_HOME`. This binary never calls `Config::load` at all (the
//! fixture config is built in memory), so only `HOME`/`XDG_DATA_HOME` are
//! redirected — there is no config-home discovery here to also isolate.
//!
//! The fixture is one loopback socket playing three parts by path: an RFC
//! 8414 authorization server (`/.well-known/oauth-authorization-server`,
//! `/token`), and the MCP resource server itself (`/mcp`), gated on
//! `Authorization: Bearer <the token `/token` most recently issued>`. No
//! `/register`: a server naming no registration endpoint is the fallback
//! client id path, already exercised in `ganja-provider`'s own unit tests —
//! this file's job is the seam those tests cannot reach, the credential
//! store and a real dial.

use std::{
    collections::BTreeMap,
    env,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ganja_core::{McpServers, McpStatus, config::McpServer};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};

/// A token that must never appear in anything a person or a log reads.
const CANARY: &str = "sk-canary-DO-NOT-PRINT-6620";

/// Shared state the fixture's `/token` handler advances and the `/mcp`
/// handler checks — the "current valid access token" a real authorization
/// server holds implicitly and this one holds explicitly.
#[derive(Default)]
struct FixtureState {
    /// The access token `/mcp` currently accepts. Replaced by every `/token`
    /// grant, authorization-code or refresh alike.
    valid_access: String,
    /// The refresh token a `grant_type=refresh_token` call must present.
    valid_refresh: String,
    /// How many times each grant type was actually asked for — what proves a
    /// refresh really ran rather than the token merely still working.
    code_grants: usize,
    refresh_grants: usize,
}

/// A loopback endpoint answering all three of an OAuth-gated MCP server's
/// parts. Returns its address; the fixture runs until the process ends.
async fn fixture() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the socket has an address");
    let state = Arc::new(Mutex::new(FixtureState::default()));
    let generation = Arc::new(AtomicUsize::new(0));

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let state = Arc::clone(&state);
            let generation = Arc::clone(&generation);
            tokio::spawn(serve(stream, address, state, generation));
        }
    });

    address
}

/// Serves every request on one connection until it closes — kept alive
/// across requests the way `ganja-core/tests/mcp.rs`'s own remote fixture is,
/// since a streamable-HTTP client posts more than one request per connection.
async fn serve(
    mut stream: TcpStream,
    address: SocketAddr,
    state: Arc<Mutex<FixtureState>>,
    generation: Arc<AtomicUsize>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];

    loop {
        let (head, body) = loop {
            if let Some(request) = whole(&buffer) {
                break request;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        buffer.drain(..head.len() + 4 + body.len());

        let path = head
            .lines()
            .next()
            .unwrap_or("")
            .split(' ')
            .nth(1)
            .unwrap_or("");
        let authorization = header(&head, "authorization");

        let response = match path {
            "/.well-known/oauth-authorization-server" => json_response(
                200,
                &json!({
                    "authorization_endpoint": format!("http://{address}/authorize"),
                    "token_endpoint": format!("http://{address}/token"),
                }),
            ),
            "/token" => token_response(&body, &state, &generation),
            "/mcp" => mcp_response(&body, authorization.as_deref(), &state),
            _ => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned(),
        };

        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
    }
}

/// Answers `/token`: a fresh access/refresh pair for either grant type,
/// refusing a `grant_type=refresh_token` that does not present the refresh
/// token the last grant actually issued — the same way a real authorization
/// server would refuse a rotated-away token.
fn token_response(body: &str, state: &Mutex<FixtureState>, generation: &AtomicUsize) -> String {
    let form = form_decode(body);
    let grant_type = form.get("grant_type").map(String::as_str).unwrap_or("");

    let mut state = state.lock().expect("never poisoned");
    if grant_type == "refresh_token" {
        let presented = form.get("refresh_token").map(String::as_str).unwrap_or("");
        if presented != state.valid_refresh {
            return json_response(400, &json!({ "error": "invalid_grant" }));
        }
        state.refresh_grants += 1;
    } else {
        state.code_grants += 1;
    }

    let generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let access = format!("{CANARY}-access-{generation}");
    let refresh = format!("{CANARY}-refresh-{generation}");
    state.valid_access.clone_from(&access);
    state.valid_refresh.clone_from(&refresh);

    json_response(
        200,
        &json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": 3600,
        }),
    )
}

/// Answers `/mcp`: `401` unless `authorization` names the token `/token`
/// last issued, `initialize`/`tools/list` otherwise — enough for
/// [`McpStatus::Connected`] to mean the bearer was both sent and accepted.
fn mcp_response(body: &str, authorization: Option<&str>, state: &Mutex<FixtureState>) -> String {
    let expected = format!(
        "Bearer {}",
        state.lock().expect("never poisoned").valid_access
    );
    if authorization != Some(expected.as_str()) {
        // RFC 6750 §3 requires this header on a bearer-auth 401, and it is
        // what `rmcp`'s own client reads to tell a real authorization
        // challenge apart from an ordinary error status — a 401 without it
        // is not enough to trigger the refresh-and-redial this test proves.
        return "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer\r\ncontent-length: 0\r\n\r\n"
            .to_owned();
    }

    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let Some(id) = request.get("id").cloned() else {
        // A notification (`initialized`) gets no reply body at all.
        return "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned();
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hub", "version": "0.0.0" },
        }),
        "tools/list" => json!({ "tools": [] }),
        _ => json!({}),
    };

    json_response(
        200,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn json_response(status: u16, body: &Value) -> String {
    let phrase = if status == 200 { "OK" } else { "Bad Request" };
    let body = body.to_string();

    format!(
        "HTTP/1.1 {status} {phrase}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A `grant_type=...&refresh_token=...` body, decoded into a map.
fn form_decode(body: &str) -> BTreeMap<String, String> {
    let mut decoded = BTreeMap::new();
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let value = url::form_urlencoded::parse(value.as_bytes())
                .map(|(value, _)| value.into_owned())
                .next()
                .unwrap_or_default();
            decoded.insert(key.to_owned(), value);
        }
    }

    decoded
}

/// One header's value, case-insensitively.
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// One whole request out of `buffer` — its head and its body.
fn whole(buffer: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(buffer).ok()?;
    let (head, rest) = text.split_once("\r\n\r\n")?;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    if rest.len() < length {
        return None;
    }

    Some((head.to_owned(), rest[..length].to_owned()))
}

/// Sends the browser's own callback: a raw GET at the loopback redirect
/// [`Servers::start_login`] published, carrying `code` and the `state` the
/// authorize URL echoed.
async fn answer_callback(url: &str, code: &str) {
    let parsed = url::Url::parse(url).expect("a login publishes a URL");
    let state = parsed
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("the authorize URL publishes a state");
    let redirect = parsed
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .expect("the authorize URL publishes a redirect_uri");
    let redirect = url::Url::parse(&redirect).expect("a redirect_uri is a URL");

    let mut socket = TcpStream::connect((
        redirect.host_str().expect("loopback has a host"),
        redirect.port().expect("the redirect names a port"),
    ))
    .await
    .expect("the loopback listener is bound");
    let target = format!("{}?code={code}&state={state}", redirect.path());
    socket
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("the callback is written");
    let mut drained = String::new();
    let _ = socket.read_to_string(&mut drained).await;
}

/// Polls `predicate` until it is true, or panics after 5 seconds — every
/// wait in this file is for a background task this test does not otherwise
/// synchronize with.
async fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition was never met within 5s");
}

#[tokio::test]
async fn oauth_discovers_logs_in_stores_sends_the_bearer_and_refreshes_on_a_401() {
    let home = tempfile::tempdir().expect("a temporary directory");
    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("HOME", home.path());
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
    }

    let address = fixture().await;
    let root = tempfile::tempdir().expect("a temporary directory");
    let config: BTreeMap<String, McpServer> = serde_json::from_value(json!({
        "hub": {
            "type": "remote",
            "url": format!("http://{address}/mcp"),
            "oauth": {},
        }
    }))
    .expect("the fixture is a config");

    // ---- discovery -> PKCE -> token, and storage under `mcp:<name>` ----

    let servers = McpServers::new(config.clone(), root.path());
    assert!(servers.has_oauth("hub"));

    servers.connect_all().await;
    assert!(
        matches!(
            servers.status().get("hub"),
            Some(McpStatus::Failed { error }) if error.contains("needs a login")
        ),
        "no credential is stored yet: {:?}",
        servers.status()
    );

    servers
        .start_login("hub")
        .await
        .expect("discovery and registration succeed against the fixture");
    let url = servers.login_url("hub").expect("a login is now in flight");
    assert!(!url.is_empty());
    answer_callback(&url, "the-code").await;

    wait_until(|| servers.login_url("hub").is_none()).await;
    wait_until(|| matches!(servers.status().get("hub"), Some(McpStatus::Connected))).await;

    let stored = ganja_core::auth::oauth_for("mcp:hub")
        .expect("the store reads")
        .expect("a login stored a credential");
    assert_eq!(stored.access.expose_secret(), &format!("{CANARY}-access-1"));
    assert_eq!(
        stored.refresh.expose_secret(),
        &format!("{CANARY}-refresh-1")
    );
    assert_eq!(
        stored.extra.get("token_endpoint").and_then(Value::as_str),
        Some(format!("http://{address}/token").as_str()),
        "the refresh below has to be able to find its way back here"
    );
    // No secret escapes through the type's own redaction, on the real value
    // this test just stored — not a synthetic one, unlike `ganja-provider`'s
    // own canary test.
    let rendered = format!("{stored:?}");
    assert!(
        !rendered.contains(CANARY),
        "a secret reached a Debug: {rendered}"
    );

    servers.shutdown().await;

    // ---- an access token the server no longer accepts forces a refresh,
    // and the re-dial that follows lands on the fresh one ----

    // Simulates a token revoked or rotated away server-side before its own
    // recorded `expires`: the store still calls it good, so the *server's*
    // 401 — not the stored deadline — is what has to trigger the refresh.
    let mut poisoned = stored.clone();
    poisoned.access = secrecy::SecretString::from(format!("{CANARY}-access-revoked"));
    ganja_core::auth::renew_oauth("mcp:hub", &poisoned).expect("the store accepts the overwrite");

    let servers = McpServers::new(config, root.path());
    servers.connect_all().await;

    assert_eq!(
        servers.status().get("hub"),
        Some(&McpStatus::Connected),
        "the forced refresh and the retry it earns should have landed: {:?}",
        servers.status()
    );

    let refreshed = ganja_core::auth::oauth_for("mcp:hub")
        .expect("the store reads")
        .expect("still a credential");
    assert_eq!(
        refreshed.access.expose_secret(),
        &format!("{CANARY}-access-2"),
        "the connect should be running on the token the refresh actually returned"
    );

    servers.shutdown().await;
}
