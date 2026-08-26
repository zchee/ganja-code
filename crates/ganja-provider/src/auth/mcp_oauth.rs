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
//! `oauth: {}` — `ganja_core::config::McpOauth`, a bare marker — carries
//! none to ask with. Anything past discovery → PKCE → token → refresh is a
//! named follow-up, not an oversight: per-request reactive re-authorization
//! (a `WWW-Authenticate` challenge mid-call) and resource-metadata discovery
//! both stay out.
//!
//! `rmcp` 3.1.2 — already in the workspace, for `ganja-core`'s MCP client
//! transport — ships its own OAuth client (`transport::auth`'s
//! `AuthorizationManager`: RFC 8414 discovery, RFC 7591 registration, PKCE,
//! and a `CredentialStore` trait of its own), found and deliberately not
//! used here: wiring it in would mean a second credential and state store
//! standing beside `auth.json` rather than inside it. It stays the named
//! path if either follow-up above is ever built — its own
//! `AuthorizationManager` already carries the protected-resource discovery
//! and `WWW-Authenticate`-seeded re-auth this module does not.
//!
//! **Every endpoint discovery names is validated too, not just the origin.**
//! [`Login::new`] restricts the origin discovery is asked at to `https` or
//! loopback `http`; `Login::discover` holds every endpoint it reads back —
//! `authorization_endpoint`, `token_endpoint`, `registration_endpoint` — to
//! that same rule before any of them is used, and `renew` re-checks a
//! stored credential's remembered `token_endpoint` on every renewal, because
//! a server naming an endpoint of its own choosing is exactly the server this
//! validation has to distrust. The check deliberately does not require an
//! endpoint share a host with the origin — a legitimate authorization server
//! may delegate to a separate host — only that https-or-loopback still holds.
//!
//! **Storage rides the existing store, at a reserved key.** A login here is
//! stamped under `mcp:<server>` — [`super::storage_key`] passes that prefix
//! through unchanged, it names nothing in `super::STORAGE_ALIASES` — and
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
    /// One of the endpoints a server's own metadata named — or a stored
    /// credential's remembered `token_endpoint`, re-checked on every renewal
    /// — is not somewhere tokens may travel to. [`Origin`] holds this same
    /// rule for the origin discovery is asked at; this is that rule applied
    /// to what discovery (or a prior login) named instead, so a hostile or
    /// compromised server cannot walk a code, a verifier or a refresh token
    /// off through cleartext or an arbitrary host by simply naming it in its
    /// own answer.
    ///
    /// [`Origin`]: Self::Origin
    #[error("the mcp server named a {field} that is not somewhere tokens may travel to")]
    UnsafeEndpoint {
        /// Which field named it: `authorization_endpoint`, `token_endpoint`,
        /// or `registration_endpoint`.
        field: &'static str,
    },
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
            Self::UnsafeEndpoint { field } => {
                format!("the server named a {field} that is not somewhere tokens may be sent")
            }
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

/// Refuses `endpoint` unless [`crate::provider::reachable_in_the_clear`] calls
/// it safe — [`Login::new`]'s own rule for the configured origin, applied
/// here to a URL a server's own metadata named instead. Called before
/// `endpoint` is ever handed to a request builder, so a refusal here means
/// nothing was sent there at all.
fn validate_endpoint(endpoint: &str, field: &'static str) -> Result<(), LoginError> {
    let parsed = Url::parse(endpoint).map_err(|_| LoginError::UnsafeEndpoint { field })?;
    if crate::provider::reachable_in_the_clear(&parsed) {
        Ok(())
    } else {
        Err(LoginError::UnsafeEndpoint { field })
    }
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
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::UnsafeEndpoint`] when the discovered
    /// `authorization_endpoint`, `token_endpoint` or `registration_endpoint`
    /// (when present) is not `https` or loopback `http` — checked here,
    /// before any of them is registered against, authorized at or exchanged
    /// with, so every later step in this login only ever holds endpoints
    /// this call already cleared.
    async fn discover(&self) -> Result<ServerMetadata, LoginError> {
        let url = format!("{}{DISCOVERY_PATH}", self.origin);
        let answer = send(self.client.get(&url), step::DISCOVERING).await?;
        let metadata: ServerMetadata = answer.into_json(step::DISCOVERING)?;

        validate_endpoint(&metadata.authorization_endpoint, "authorization_endpoint")?;
        validate_endpoint(&metadata.token_endpoint, "token_endpoint")?;
        if let Some(registration_endpoint) = &metadata.registration_endpoint {
            validate_endpoint(registration_endpoint, "registration_endpoint")?;
        }

        Ok(metadata)
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
/// id `credential` minted it back out of [`OauthCredential::extra`], rather
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
    // Re-checked here, not just at discovery: this credential may have been
    // written before this validation existed, and this is the one path that
    // would otherwise post a refresh token to it silently, forever.
    validate_endpoint(&token_endpoint, "token_endpoint")?;
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
#[path = "mcp_oauth_tests.rs"]
mod tests;
