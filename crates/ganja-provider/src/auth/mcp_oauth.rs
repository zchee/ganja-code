//! OAuth for a remote MCP server: discovery, dynamic registration, then the
//! same PKCE and loopback machinery every browser login here already uses.
//!
//! Spec: no upstream `opencode` TypeScript file — the checkout refuses the
//! config key this unlocks by name rather than reading it — so what this
//! ports instead is [MCP's own authorization spec][mcp-auth], the way Claude
//! Code's `/mcp` "Login" affordance is read to work (**D466**,
//! `mcp-oauth-is-a-claude-port`).
//!
//! [mcp-auth]: https://modelcontextprotocol.io/specification/2025-06-18/basic/authorization
//!
//! Four steps, the last two reused rather than rewritten: RFC 8414
//! authorization-server discovery against the MCP server's own origin, RFC
//! 7591 dynamic client registration — minimal, and falling back to a fixed
//! public client id when a server names no registration endpoint at all —
//! then [`super::pkce`] and [`super::loopback`] exactly as
//! [`super::openai`]'s browser login already runs them; neither module knows
//! or cares which provider is asking.
//!
//! **Deliberately minimal**, matching the wave that shipped it. Discovery is
//! one request against the origin, not the resource-metadata-first,
//! path-aware search newer MCP revisions describe; registration asks for
//! nothing beyond what RFC 7591 requires; no scope is ever requested, because
//! `oauth: {}` — [`ganja_core::config::McpOauth`], a bare marker — carries
//! none to ask with. Anything past discovery → PKCE → token → refresh is a
//! named follow-up, not an oversight: per-request reactive re-authorization
//! (a `WWW-Authenticate` challenge mid-call) and resource-metadata discovery
//! both stay out.
//!
//! **Storage rides the existing store, at a reserved key.** A login here is
//! stamped under `mcp:<server>` — [`super::storage_key`] passes that prefix
//! through unchanged, it names nothing in [`super::STORAGE_ALIASES`] — and
//! [`OauthCredential::extra`] carries two fields no other login writes:
//! `token_endpoint` and `client_id`, discovered and registered once and read
//! back by every [`Refresher::refresh`], because a refresh has to ask the
//! same endpoint under the same client the authorization was granted to, and
//! nothing else in the stored record says which endpoint that was.
//!
//! **Nothing here writes to the credential store**, matching every sibling
//! login in this module: [`Browser::wait`] and [`Refresher::refresh`] return
//! a credential or an error, and the caller decides whether and how to store
//! it.

use std::{fmt, time::Duration};

use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::{Url, form_urlencoded};

use super::{
    AuthError, OauthCredential, RefreshOauth,
    loopback::{self, LoopbackError},
    now_ms,
    pkce::{self, EntropyError, Pkce},
};

/// How long a browser login waits for the callback. MCP's own authorization
/// spec names no deadline, so this is [`super::openai::CALLBACK_DEADLINE`]'s
/// bound applied here too.
pub const CALLBACK_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// The path the loopback redirect is registered at.
const CALLBACK_PATH: &str = "/callback";

/// RFC 8414's well-known path, fetched at the server's own origin.
const DISCOVERY_PATH: &str = "/.well-known/oauth-authorization-server";

/// What this build calls itself: RFC 7591's `client_name`.
const CLIENT_NAME: &str = "ganja";

/// The client id presented when a server's metadata names no registration
/// endpoint at all. `oauth: {}` is a bare marker with nothing configurable
/// yet (see [`ganja_core::config::McpOauth`]'s own doc), so there is no
/// per-server id to fall back to instead — this is fixed and public, the way
/// every other login's own `CLIENT_ID` in this module is: a value that
/// identifies *this build* to a server, not a secret.
const CLIENT_ID_FALLBACK: &str = "ganja-mcp-client";

/// What a form-encoded body is announced as.
const FORM: &str = "application/x-www-form-urlencoded";

/// One request's whole budget: connect, headers and body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an access token lasts when the token endpoint did not say.
const DEFAULT_EXPIRES_IN: u64 = 3600;

/// The two extras this login writes into [`OauthCredential::extra`], read
/// back by [`Refresher::refresh`] — see this module's own "Storage" section.
mod extra {
    pub(super) const TOKEN_ENDPOINT: &str = "token_endpoint";
    pub(super) const CLIENT_ID: &str = "client_id";
}

/// Naming the step a failure happened at, for the message a person reads.
mod step {
    pub(super) const DISCOVERING: &str = "discovering the server's authorization endpoints";
    pub(super) const REGISTERING: &str = "registering a client";
    pub(super) const EXCHANGING: &str = "exchanging the authorization code";
    pub(super) const RENEWING: &str = "renewing the credential";
}

/// An MCP server login, or its renewal, could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The server's URL is not somewhere tokens may travel to — not `https`,
    /// or not loopback `http`, or an origin a secret could hide inside.
    #[error(
        "mcp server oauth needs an https origin, or http to loopback; \
         anything else puts the login's tokens on the wire in the clear"
    )]
    Origin,
    /// No HTTP client could be built.
    #[error("no HTTP client for the mcp server login: {source}")]
    Client {
        /// What the client builder said.
        #[source]
        source: reqwest::Error,
    },
    /// The unguessable values a login needs could not be drawn.
    #[error(transparent)]
    Entropy {
        /// What the platform's random source said.
        source: EntropyError,
    },
    /// The browser's callback did not arrive, or did not belong to this
    /// login.
    #[error(transparent)]
    Callback {
        /// What the listener said.
        source: LoopbackError,
    },
    /// The server could not be reached.
    #[error("the mcp server could not be reached while {step}: {source}")]
    Unreachable {
        /// What was being attempted.
        step: &'static str,
        /// What the transport said, without its URL.
        #[source]
        source: reqwest::Error,
    },
    /// The server answered, and refused.
    #[error("the mcp server refused while {step}: HTTP {status}")]
    Refused {
        /// What was being attempted.
        step: &'static str,
        /// The status it answered with.
        status: u16,
    },
    /// The server answered with something this cannot use.
    ///
    /// The decoder's own message is thrown away: every value in these answers
    /// may be a token, and `serde_json` quotes the offending value back.
    #[error("the mcp server's answer while {step} was not the shape a login expects")]
    Malformed {
        /// What was being attempted.
        step: &'static str,
    },
    /// A stored credential named no token endpoint to renew at — it was not
    /// written by this module, or was written before this module existed.
    #[error("the stored mcp credential names no token endpoint to renew it at")]
    NoTokenEndpoint,
    /// Nobody completed the authorization in time.
    #[error("the mcp login was not completed within {}s", .after.as_secs())]
    TimedOut {
        /// How long was allowed.
        after: Duration,
    },
    /// The login was cancelled.
    #[error("the mcp login was cancelled")]
    Cancelled,
}

impl From<EntropyError> for LoginError {
    fn from(source: EntropyError) -> Self {
        Self::Entropy { source }
    }
}

impl From<LoopbackError> for LoginError {
    /// Keeps one spelling per outcome, the way [`super::openai`]'s own
    /// conversion does — a login that ran out of time or was cancelled says
    /// so the same way whichever flow it was.
    fn from(error: LoopbackError) -> Self {
        match error {
            LoopbackError::TimedOut { after } => Self::TimedOut { after },
            LoopbackError::Cancelled => Self::Cancelled,
            source => Self::Callback { source },
        }
    }
}

impl LoginError {
    /// This failure as the store's own vocabulary, for `provider_id`.
    fn into_auth(self, provider_id: &str) -> AuthError {
        let provider_id = provider_id.to_owned();
        let dead = matches!(&self, Self::Refused { status, .. } if is_dead_grant(*status));
        let reason = self.reason();

        if dead {
            AuthError::ReauthRequired {
                provider_id,
                reason,
            }
        } else {
            AuthError::RefreshUnavailable {
                provider_id,
                reason,
            }
        }
    }

    /// Why, in a few words that carry no token and no response body.
    fn reason(&self) -> String {
        match self {
            Self::Refused { status, .. } => format!("HTTP {status}"),
            Self::Unreachable { source, .. } => format!("the server was not reachable: {source}"),
            Self::Malformed { .. } => {
                "the server's answer was not the shape a renewal has".to_owned()
            }
            Self::Origin => "the server's origin is not somewhere tokens may be sent".to_owned(),
            Self::Client { source } => format!("no HTTP client: {source}"),
            Self::Entropy { source } => source.to_string(),
            Self::Callback { source } => source.to_string(),
            Self::NoTokenEndpoint => "no token endpoint was recorded for this login".to_owned(),
            Self::TimedOut { after } => format!("it did not answer within {}s", after.as_secs()),
            Self::Cancelled => "it was cancelled".to_owned(),
        }
    }
}

/// Whether a status from the token endpoint means the grant itself is gone —
/// [`super::openai`]'s own reading of RFC 6749 §5.2, repeated here rather than
/// shared: each login owns its own classification, and the two happen to
/// agree.
fn is_dead_grant(status: u16) -> bool {
    (400..500).contains(&status) && status != 429
}

/// RFC 8414's authorization-server metadata, narrowed to what this login
/// needs. Not `deny_unknown_fields`: a real server's metadata carries many
/// more fields (RFC 8414 §2) this build has no use for, and ignoring them is
/// correct.
#[derive(Debug, Deserialize)]
struct ServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
}

/// RFC 7591's registration response, narrowed to the one field this login
/// reads back.
#[derive(Debug, Deserialize)]
struct ClientRegistration {
    client_id: String,
}

/// What a token endpoint answers with, on an exchange or a renewal.
#[derive(Deserialize)]
struct Tokens {
    access_token: SecretString,
    /// Absent on a renewal that did not rotate.
    #[serde(default)]
    refresh_token: Option<SecretString>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// A response, before anything has judged it. [`super::openai::Answer`]'s
/// twin: read once as text so an unreachable server and an unparseable
/// answer are never the same failure.
struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    fn accepted(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn into_json<T: serde::de::DeserializeOwned>(
        self,
        step: &'static str,
    ) -> Result<T, LoginError> {
        if !self.accepted() {
            return Err(LoginError::Refused {
                step,
                status: self.status,
            });
        }

        serde_json::from_str(&self.body).map_err(|_| LoginError::Malformed { step })
    }
}

/// Sends one request and reads its answer as text, judging only the
/// transport: a failure to reach the server at all is [`LoginError::Unreachable`]
/// and can be nothing else.
async fn send(request: reqwest::RequestBuilder, step: &'static str) -> Result<Answer, LoginError> {
    let unreachable = |source: reqwest::Error| LoginError::Unreachable {
        step,
        source: source.without_url(),
    };

    let response = request.send().await.map_err(unreachable)?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(unreachable)?;

    Ok(Answer { status, body })
}

/// A form-encoded `POST`, the shape every login in this crate builds one.
fn form_post(
    client: &reqwest::Client,
    url: &str,
    pairs: &[(&str, &str)],
) -> reqwest::RequestBuilder {
    client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, FORM)
        .body(super::device::form(pairs))
}

/// A login against one MCP server's own authorization server.
#[derive(Clone, Debug)]
pub struct Login {
    /// The server's origin, without a trailing slash or a path — what RFC
    /// 8414 discovery is asked at.
    origin: String,
    client: reqwest::Client,
}

impl Login {
    /// A login against the origin `server_url` names.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Origin`] when `server_url` does not parse, is
    /// not `https` or loopback `http`, or is not a tuple origin (`https`/
    /// `http`), and [`LoginError::Client`] when no HTTP client can be built.
    pub fn new(server_url: &str) -> Result<Self, LoginError> {
        let parsed = Url::parse(server_url).map_err(|_| LoginError::Origin)?;
        if !crate::provider::reachable_in_the_clear(&parsed) {
            return Err(LoginError::Origin);
        }
        let origin = parsed.origin();
        if !origin.is_tuple() {
            return Err(LoginError::Origin);
        }

        let client =
            super::login_client(REQUEST_TIMEOUT).map_err(|source| LoginError::Client { source })?;

        Ok(Self {
            origin: origin.ascii_serialization(),
            client,
        })
    }

    /// Starts a browser login: binds the loopback redirect, discovers the
    /// server's endpoints, registers a client, and builds the URL to open.
    ///
    /// The socket is bound before discovery runs, for the same reason
    /// [`super::openai::Login::browser_on`]'s is: the registration below
    /// needs the redirect URI to name the port that was actually got, and a
    /// browser opened before anything is listening can finish the login into
    /// a closed port.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Callback`] when the loopback port cannot be
    /// bound, [`LoginError::Entropy`] when the platform has no entropy, and
    /// discovery's or registration's own failures otherwise.
    pub async fn browser(&self) -> Result<Browser, LoginError> {
        let listener = loopback::Listener::bind(0).await?;
        let redirect = format!("http://127.0.0.1:{}{CALLBACK_PATH}", listener.port());

        let metadata = self.discover().await?;
        let client_id = self.register(&metadata, &redirect).await?;

        let pkce = Pkce::generate()?;
        let state = pkce::unguessable()?;
        let url = authorize_url(
            &metadata.authorization_endpoint,
            &client_id,
            &redirect,
            pkce.challenge(),
            state.expose_secret(),
        );

        Ok(Browser {
            login: self.clone(),
            url,
            redirect,
            listener,
            pkce,
            state,
            token_endpoint: metadata.token_endpoint,
            client_id,
        })
    }

    /// RFC 8414 discovery at this login's origin.
    async fn discover(&self) -> Result<ServerMetadata, LoginError> {
        let url = format!("{}{DISCOVERY_PATH}", self.origin);
        let answer = send(self.client.get(&url), step::DISCOVERING).await?;

        answer.into_json(step::DISCOVERING)
    }

    /// RFC 7591 dynamic client registration, minimal — or the fixed fallback
    /// id, when `metadata` names no registration endpoint at all.
    async fn register(
        &self,
        metadata: &ServerMetadata,
        redirect: &str,
    ) -> Result<String, LoginError> {
        let Some(endpoint) = &metadata.registration_endpoint else {
            return Ok(CLIENT_ID_FALLBACK.to_owned());
        };

        let answer = send(
            self.client.post(endpoint).json(&json!({
                "client_name": CLIENT_NAME,
                "redirect_uris": [redirect],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })),
            step::REGISTERING,
        )
        .await?;
        let registered: ClientRegistration = answer.into_json(step::REGISTERING)?;

        Ok(registered.client_id)
    }

    /// Trades an authorization code for tokens.
    async fn exchange(
        &self,
        code: &SecretString,
        redirect: &str,
        verifier: &SecretString,
        client_id: &str,
        token_endpoint: &str,
    ) -> Result<Tokens, LoginError> {
        let answer = send(
            form_post(
                &self.client,
                token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code.expose_secret()),
                    ("redirect_uri", redirect),
                    ("client_id", client_id),
                    ("code_verifier", verifier.expose_secret()),
                ],
            ),
            step::EXCHANGING,
        )
        .await?;

        answer.into_json(step::EXCHANGING)
    }
}

/// Where the person is sent, built from what discovery and registration
/// found. No `scope` parameter: `oauth: {}` carries none to ask with — see
/// this module's own doc.
fn authorize_url(
    endpoint: &str,
    client_id: &str,
    redirect: &str,
    challenge: &str,
    state: &str,
) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .finish();
    let separator = if endpoint.contains('?') { '&' } else { '?' };

    format!("{endpoint}{separator}{query}")
}

/// A browser login whose listener is bound and whose URL is ready to open.
pub struct Browser {
    login: Login,
    url: String,
    /// What the server was told to redirect to, repeated at the exchange
    /// because RFC 6749 §4.1.3 requires the two to match.
    redirect: String,
    listener: loopback::Listener,
    /// The proof that the code being exchanged was issued to this login.
    pkce: Pkce,
    /// The proof that the callback belongs to this login.
    state: SecretString,
    /// Where the exchange, and every later refresh, spends its tokens.
    token_endpoint: String,
    /// What this login registered, or the fixed fallback.
    client_id: String,
}

impl fmt::Debug for Browser {
    /// Hand-written for the reason [`super::openai::Browser`]'s is: [`url`]
    /// carries the `state` in its query, and this type is held for the length
    /// of a login.
    ///
    /// [`url`]: Self::url
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Browser")
            .field("port", &self.listener.port())
            .field("challenge", &self.pkce.challenge())
            .field("redirect", &self.redirect)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl Browser {
    /// Where the person has to go. Print it; open it if there is a browser.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The port the callback will arrive on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.listener.port()
    }

    /// Waits for the callback and exchanges what it carried.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError`]: [`Callback`] when the redirect was refused,
    /// forged or empty, [`TimedOut`] or [`Cancelled`] when nobody finished,
    /// the server's own failures from the exchange, and [`Malformed`] when
    /// the exchange carried no refresh token — a credential without one dies
    /// at the end of its own access token's lifetime with no way back.
    ///
    /// [`Callback`]: LoginError::Callback
    /// [`TimedOut`]: LoginError::TimedOut
    /// [`Cancelled`]: LoginError::Cancelled
    /// [`Malformed`]: LoginError::Malformed
    pub async fn wait(
        self,
        within: Duration,
        cancel: &CancellationToken,
    ) -> Result<OauthCredential, LoginError> {
        let code = self
            .listener
            .wait(CALLBACK_PATH, &self.state, within, cancel)
            .await?;
        let tokens = self
            .login
            .exchange(
                &code,
                &self.redirect,
                self.pkce.verifier(),
                &self.client_id,
                &self.token_endpoint,
            )
            .await?;

        first_credential(
            tokens,
            &self.token_endpoint,
            &self.client_id,
            step::EXCHANGING,
        )
    }
}

/// The credential a *login's* token response becomes. Requires a refresh
/// token, the way [`super::openai::first_credential`] does and for the same
/// reason.
fn first_credential(
    tokens: Tokens,
    token_endpoint: &str,
    client_id: &str,
    step: &'static str,
) -> Result<OauthCredential, LoginError> {
    let refresh = tokens
        .refresh_token
        .clone()
        .ok_or(LoginError::Malformed { step })?;

    Ok(credential(tokens, refresh, token_endpoint, client_id))
}

/// The credential a token response becomes, given the refresh token to keep
/// and the endpoint/client id every later refresh needs back.
fn credential(
    tokens: Tokens,
    refresh: SecretString,
    token_endpoint: &str,
    client_id: &str,
) -> OauthCredential {
    let mut credential =
        OauthCredential::new(refresh, tokens.access_token, expires(tokens.expires_in));
    credential.extra.insert(
        extra::TOKEN_ENDPOINT.to_owned(),
        serde_json::Value::from(token_endpoint),
    );
    credential.extra.insert(
        extra::CLIENT_ID.to_owned(),
        serde_json::Value::from(client_id),
    );

    credential
}

/// When an access token stops being accepted.
fn expires(expires_in: Option<u64>) -> u64 {
    now_ms().saturating_add(
        expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN)
            .saturating_mul(1_000),
    )
}

/// One extra field [`credential`] wrote, read back out of a stored
/// credential's [`OauthCredential::extra`].
fn extra_str(credential: &OauthCredential, key: &str) -> Option<String> {
    credential.extra.get(key)?.as_str().map(str::to_owned)
}

/// Renews a stored MCP-server credential by reading the endpoint and client
/// id [`credential`] minted it back out of [`OauthCredential::extra`], rather
/// than holding either itself — one stateless value serves every
/// oauth-configured server this way, and it is what
/// [`ganja_core::mcp`](../../../ganja_core/mcp/index.html) calls both to
/// refresh a token nearing its recorded deadline and to force a refresh a
/// server's own 401 asked for regardless of that deadline.
#[derive(Clone, Copy, Debug, Default)]
pub struct Refresher;

#[async_trait::async_trait]
impl RefreshOauth for Refresher {
    /// # Errors
    ///
    /// Returns [`AuthError::RefreshUnavailable`] when `credential` names no
    /// token endpoint (it was not written by this module) or the renewal
    /// could not be reached or understood, and [`AuthError::ReauthRequired`]
    /// when the server refused the refresh token outright.
    async fn refresh(
        &self,
        provider_id: &str,
        credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        renew(credential)
            .await
            .map_err(|error| error.into_auth(provider_id))
    }
}

/// The renewal [`Refresher::refresh`] runs.
async fn renew(previous: &OauthCredential) -> Result<OauthCredential, LoginError> {
    let token_endpoint =
        extra_str(previous, extra::TOKEN_ENDPOINT).ok_or(LoginError::NoTokenEndpoint)?;
    let client_id =
        extra_str(previous, extra::CLIENT_ID).unwrap_or_else(|| CLIENT_ID_FALLBACK.to_owned());
    let client =
        super::login_client(REQUEST_TIMEOUT).map_err(|source| LoginError::Client { source })?;

    let answer = send(
        form_post(
            &client,
            &token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", previous.refresh.expose_secret()),
                ("client_id", &client_id),
            ],
        ),
        step::RENEWING,
    )
    .await?;
    let tokens: Tokens = answer.into_json(step::RENEWING)?;

    // A renewal that did not rotate the refresh token has not revoked the
    // one that was sent — keeping it is what lets the *next* renewal happen
    // at all, the same reasoning `openai::Login::renew` is built on.
    let refresh = tokens
        .refresh_token
        .clone()
        .unwrap_or_else(|| previous.refresh.clone());

    Ok(credential(tokens, refresh, &token_endpoint, &client_id).inheriting(previous))
}

#[cfg(test)]
mod tests {
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
    /// and a test proves that by turning this off.
    async fn authorization_server(
        with_registration: bool,
    ) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port is available");
        let address = listener.local_addr().expect("the socket has an address");
        let seen_client_ids: Arc<Mutex<Vec<String>>> = Arc::default();

        let recorded = Arc::clone(&seen_client_ids);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = Arc::clone(&recorded);
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

        (address, seen_client_ids)
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
        let (address, seen_client_ids) = authorization_server(true).await;
        let login =
            Login::new(&format!("http://{address}/mcp")).expect("a loopback origin logs in");
        let browser = login
            .browser()
            .await
            .expect("discovery and registration succeed");
        let url = browser.url().to_owned();

        let waited =
            tokio::spawn(async move { browser.wait(AMPLE, &CancellationToken::new()).await });
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
        let (address, seen_client_ids) = authorization_server(false).await;
        let login =
            Login::new(&format!("http://{address}/mcp")).expect("a loopback origin logs in");
        let browser = login
            .browser()
            .await
            .expect("discovery succeeds without registration");
        let url = browser.url().to_owned();

        assert!(url.contains("client_id=ganja-mcp-client"), "{url}");

        let waited =
            tokio::spawn(async move { browser.wait(AMPLE, &CancellationToken::new()).await });
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
        let (address, seen_client_ids) = authorization_server(true).await;
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
}
