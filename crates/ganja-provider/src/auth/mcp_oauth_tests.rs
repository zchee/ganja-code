use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

use super::{Login, LoginError, RefreshOauth as _, Refresher};
use crate::auth::{AuthErrorKind, OauthCredential};

/// Long enough that a loaded machine still gets there; short enough that
/// a hung test fails promptly.
const AMPLE: Duration = Duration::from_secs(20);

/// A token that must never appear in anything a person or a log reads.
const CANARY: &str = "sk-canary-DO-NOT-PRINT-7734";

/// A minimal RFC 8414 + 7591 + token-endpoint authorization server, over a
/// real loopback socket — the same hand-rolled-HTTP posture
/// `ganja-core`'s own MCP fixtures use, and for the same reason: what is
/// under test is the request this login actually builds.
///
/// `with_registration` toggles whether `/register` is advertised — the
/// registration-endpoint-absent path falls back to the fixed client id,
/// and a test proves that by turning this off. `poison`, when set,
/// overwrites one field of the discovery response with a value of the
/// caller's choosing — how the endpoint-validation tests build a metadata
/// answer a login has to refuse to trust. The third return value is every
/// path this server actually received a request for, in arrival order —
/// the poisoned-endpoint tests read it to confirm a refused discovery
/// stops a login before registration or a token request ever runs.
async fn authorization_server(
    with_registration: bool,
    poison: Option<(&'static str, String)>,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the socket has an address");
    let seen_client_ids: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen_paths: Arc<Mutex<Vec<String>>> = Arc::default();

    let recorded = Arc::clone(&seen_client_ids);
    let all_paths = Arc::clone(&seen_paths);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            let all_paths = Arc::clone(&all_paths);
            let poison = poison.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let (head, body) = loop {
                    if let Some(request) = whole(&buffer) {
                        break request;
                    }
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                };

                let path = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split(' ')
                    .nth(1)
                    .unwrap_or("");
                all_paths
                    .lock()
                    .expect("never poisoned")
                    .push(path.to_owned());
                let response = if path == "/.well-known/oauth-authorization-server" {
                    let mut metadata = json!({
                        "issuer": format!("http://{address}"),
                        "authorization_endpoint": format!("http://{address}/authorize"),
                        "token_endpoint": format!("http://{address}/token"),
                    });
                    if with_registration {
                        metadata["registration_endpoint"] =
                            json!(format!("http://{address}/register"));
                    }
                    if let Some((field, value)) = poison {
                        metadata[field] = json!(value);
                    }
                    json_response(200, &metadata)
                } else if path == "/register" {
                    json_response(201, &json!({ "client_id": "dcr-registered-client" }))
                } else if path == "/token" {
                    let request: Value =
                        serde_json::from_str(&body).unwrap_or_else(|_| form_decode(&body));
                    if let Some(client_id) = request.get("client_id").and_then(Value::as_str) {
                        recorded
                            .lock()
                            .expect("never poisoned")
                            .push(client_id.to_owned());
                    }
                    json_response(
                        200,
                        &json!({
                            "access_token": format!("{CANARY}-access"),
                            "refresh_token": format!("{CANARY}-refresh"),
                            "expires_in": 3600,
                        }),
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned()
                };

                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (address, seen_client_ids, seen_paths)
}

fn json_response(status: u16, body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A `grant_type=...&client_id=...` body, read as if it were the JSON this
/// fixture otherwise expects — good enough to pull `client_id` back out
/// for the assertion that cares which one was sent.
fn form_decode(body: &str) -> Value {
    let mut object = serde_json::Map::new();
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            object.insert(key.to_owned(), Value::from(value.to_owned()));
        }
    }

    Value::Object(object)
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

/// Sends the browser's own callback: a raw GET at the loopback redirect,
/// carrying the `state` published in the authorize URL.
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

    let mut socket = tokio::net::TcpStream::connect((
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

#[tokio::test]
async fn discovery_registration_and_the_exchange_complete_end_to_end() {
    let (address, seen_client_ids, _seen_paths) = authorization_server(true, None).await;
    let login = Login::new(&format!("http://{address}/mcp")).expect("a loopback origin logs in");
    let browser = login
        .browser()
        .await
        .expect("discovery and registration succeed");
    let url = browser.url().to_owned();

    let waited = tokio::spawn(async move { browser.wait(AMPLE, &CancellationToken::new()).await });
    answer_callback(&url, "the-code").await;
    let credential = waited
        .await
        .expect("the wait finished")
        .expect("the exchange succeeds");

    assert_eq!(
        credential.access.expose_secret(),
        &format!("{CANARY}-access")
    );
    assert_eq!(
        credential.refresh.expose_secret(),
        &format!("{CANARY}-refresh")
    );
    assert_eq!(
        credential
            .extra
            .get("token_endpoint")
            .and_then(Value::as_str),
        Some(format!("http://{address}/token").as_str())
    );
    assert_eq!(
        credential.extra.get("client_id").and_then(Value::as_str),
        Some("dcr-registered-client")
    );
    assert_eq!(
        seen_client_ids.lock().expect("never poisoned").as_slice(),
        ["dcr-registered-client"],
        "the exchange has to present the id registration actually returned"
    );
}

#[tokio::test]
async fn a_server_with_no_registration_endpoint_gets_the_fixed_fallback_id() {
    let (address, seen_client_ids, _seen_paths) = authorization_server(false, None).await;
    let login = Login::new(&format!("http://{address}/mcp")).expect("a loopback origin logs in");
    let browser = login
        .browser()
        .await
        .expect("discovery succeeds without registration");
    let url = browser.url().to_owned();

    assert!(url.contains("client_id=ganja-mcp-client"), "{url}");

    let waited = tokio::spawn(async move { browser.wait(AMPLE, &CancellationToken::new()).await });
    answer_callback(&url, "the-code").await;
    waited
        .await
        .expect("the wait finished")
        .expect("the exchange still succeeds");

    assert_eq!(
        seen_client_ids.lock().expect("never poisoned").as_slice(),
        ["ganja-mcp-client"]
    );
}

#[tokio::test]
async fn a_refresh_reads_the_endpoint_and_client_id_the_login_stored() {
    let (address, seen_client_ids, _seen_paths) = authorization_server(true, None).await;
    let mut stored = OauthCredential::new(
        secrecy::SecretString::from(format!("{CANARY}-old-refresh")),
        secrecy::SecretString::from(format!("{CANARY}-old-access")),
        0,
    );
    stored.extra.insert(
        "token_endpoint".to_owned(),
        Value::from(format!("http://{address}/token")),
    );
    stored
        .extra
        .insert("client_id".to_owned(), Value::from("dcr-registered-client"));

    let renewed = Refresher
        .refresh("mcp:fixture", &stored)
        .await
        .expect("the fixture's token endpoint answers a refresh");

    assert_eq!(renewed.access.expose_secret(), &format!("{CANARY}-access"));
    assert_eq!(
        seen_client_ids.lock().expect("never poisoned").as_slice(),
        ["dcr-registered-client"],
        "a refresh has to authenticate as the client the grant belongs to"
    );
    assert_eq!(
        renewed.extra.get("token_endpoint").and_then(Value::as_str),
        Some(format!("http://{address}/token").as_str()),
        "the endpoint travels forward so the *next* refresh still knows where to ask"
    );
}

#[tokio::test]
async fn a_credential_this_login_never_wrote_cannot_be_refreshed() {
    let bare = OauthCredential::new(
        secrecy::SecretString::from("r"),
        secrecy::SecretString::from("a"),
        0,
    );

    let error = Refresher
        .refresh("mcp:fixture", &bare)
        .await
        .expect_err("no token endpoint was ever recorded");
    assert_eq!(error.kind(), AuthErrorKind::RefreshUnavailable);
}

#[tokio::test]
async fn discovery_naming_an_unsafe_token_endpoint_is_refused_before_any_request_reaches_it() {
    let (address, seen_client_ids, seen_paths) = authorization_server(
        true,
        Some((
            "token_endpoint",
            "http://mcp-attacker.example/token".to_owned(),
        )),
    )
    .await;
    let login = Login::new(&format!("http://{address}/mcp")).expect("a loopback origin logs in");

    let result = login.browser().await;

    assert!(
        matches!(
            result,
            Err(LoginError::UnsafeEndpoint {
                field: "token_endpoint"
            })
        ),
        "{result:?}"
    );
    assert!(
        seen_client_ids.lock().expect("never poisoned").is_empty(),
        "nothing was ever posted to the poisoned token endpoint"
    );
    assert_eq!(
        seen_paths.lock().expect("never poisoned").as_slice(),
        ["/.well-known/oauth-authorization-server"],
        "a refused discovery must stop the login before registration ever runs"
    );
}

#[tokio::test]
async fn discovery_naming_an_unsafe_authorization_or_registration_endpoint_is_refused_too() {
    for field in ["authorization_endpoint", "registration_endpoint"] {
        let (address, _seen_client_ids, seen_paths) = authorization_server(
            true,
            Some((field, "http://mcp-attacker.example/evil".to_owned())),
        )
        .await;
        let login =
            Login::new(&format!("http://{address}/mcp")).expect("a loopback origin logs in");

        let result = login.browser().await;

        assert!(
            matches!(result, Err(LoginError::UnsafeEndpoint { field: got }) if got == field),
            "{field}: {result:?}"
        );
        assert_eq!(
            seen_paths.lock().expect("never poisoned").as_slice(),
            ["/.well-known/oauth-authorization-server"],
            "{field}: a refused discovery must stop the login before registration ever runs"
        );
    }
}

#[tokio::test]
async fn a_stored_unsafe_token_endpoint_refuses_a_renewal_without_sending_the_refresh_token() {
    let mut stored = OauthCredential::new(
        secrecy::SecretString::from(format!("{CANARY}-old-refresh")),
        secrecy::SecretString::from(format!("{CANARY}-old-access")),
        0,
    );
    stored.extra.insert(
        "token_endpoint".to_owned(),
        Value::from("http://mcp-attacker.example/token"),
    );
    stored
        .extra
        .insert("client_id".to_owned(), Value::from("dcr-registered-client"));

    // The refusal happens before any HTTP client is even built for the
    // renewal — a real attempt at "mcp-attacker.example" would need at
    // least a DNS lookup and a connect, neither of which finishes in
    // milliseconds; this bounds the call tightly enough that only the
    // synchronous validation path could possibly answer in time.
    let outcome = tokio::time::timeout(
        Duration::from_millis(500),
        Refresher.refresh("mcp:fixture", &stored),
    )
    .await;

    let error = outcome
        .expect("a refused endpoint is caught well before any network attempt")
        .expect_err("the poisoned token endpoint must be refused");
    assert_eq!(error.kind(), AuthErrorKind::RefreshUnavailable);
    assert!(
        error.to_string().contains("token_endpoint"),
        "the refusal must name the field: {error}"
    );
    assert!(
        !error.to_string().contains(CANARY),
        "a secret reached a message: {error}"
    );
}

#[test]
fn an_origin_that_would_put_the_tokens_in_the_clear_is_refused() {
    for allowed in [
        "https://mcp.example/mcp",
        "http://127.0.0.1:8080/mcp",
        "http://localhost:9/sse",
    ] {
        assert!(Login::new(allowed).is_ok(), "{allowed}");
    }
    for refused in [
        "http://mcp.example/mcp",
        "http://mcp.example.invalid/mcp",
        "ftp://mcp.example/mcp",
        "not a url",
    ] {
        assert!(
            matches!(Login::new(refused), Err(LoginError::Origin)),
            "{refused}"
        );
    }
}

#[test]
fn no_failure_message_or_debug_renders_a_token() {
    let messages = [
        LoginError::Refused {
            step: "renewing the credential",
            status: 401,
        }
        .into_auth("mcp:fixture")
        .to_string(),
        LoginError::Malformed {
            step: "exchanging the authorization code",
        }
        .into_auth("mcp:fixture")
        .to_string(),
        format!(
            "{:?}",
            Login::new("https://mcp.example/mcp").expect("https is allowed")
        ),
    ];

    for message in messages {
        assert!(
            !message.contains(CANARY),
            "a secret reached a message: {message}"
        );
    }
}
