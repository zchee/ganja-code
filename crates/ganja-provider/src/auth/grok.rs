//! Logging in to xAI, with a browser on this machine or without one, and
//! staying logged in.
//!
//! Spec: upstream `packages/opencode/src/plugin/xai.ts`. Two of its three login
//! methods are here. The PKCE loopback method (`:551-584`) sends a browser on
//! this machine to xAI and catches the redirect on a socket of its own; the
//! device grant (`:585-618`) asks nothing of this machine at all, which is what
//! makes it the one that works over SSH, in a container, and where the browser
//! is somewhere else entirely. The third method — "manually enter API Key"
//! (`:619-622`) — is what `ganja auth login` already does.
//!
//! The two flows differ only in how the grant is obtained. Everything after the
//! tokens arrive is one code path: one [`credential_from`], one [`Refresh`],
//! one entry in `auth.json`. That is why the loopback flow hands back
//! [`Tokens`] — the device flow's own type — rather than a credential of its
//! own shape, and it is what makes "both logins store the same bytes" a fact
//! about which function runs rather than two implementations agreeing.
//!
//! **The provider is called `grok` here and `xai` on disk.** That is not an
//! inconsistency to tidy up: [`super::storage_key`] maps the one to the other
//! so that an `auth.json` shared with an opencode install keeps working, and
//! [`PROVIDER_ID`] is the single place ganja's own name for it is written.
//!
//! Nothing in this module writes a credential. A login hands back an
//! [`OauthCredential`] and the caller stores it, which is what makes a
//! cancelled login structurally unable to leave anything behind.

use std::{fmt, io, time::Duration};

use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

use super::{
    AuthError, OauthCredential,
    device::{
        BodyEncoding, DeviceError, DeviceFlow, Tokens, UPSTREAM_USER_AGENT, form, json_object,
        positive_seconds_ms, reportable_code, text,
    },
    loopback::{self, LoopbackError},
    now_ms,
    pkce::{self, EntropyError, Pkce},
};

/// What ganja calls this provider, everywhere but in the credential file.
///
/// The one source of the string: a command-line argument, a config key and a
/// provider id all mean the same provider, and three literals agreeing by
/// accident is one of them being wrong.
pub const PROVIDER_ID: &str = "grok";

/// The OAuth client the login presents itself as.
///
/// Upstream's, and upstream's comment explains why it is not opencode's own
/// (`xai.ts:7-10`): "xAI's auth server rejects loopback OAuth from
/// non-allowlisted clients, so we reuse the Grok-CLI client_id that xAI ships
/// for desktop OAuth flows."
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// Where an authorization is started (`xai.ts:20`).
const DEVICE_AUTHORIZATION_URL: &str = "https://auth.x.ai/oauth2/device/code";

/// Where a browser is sent to authorize (`xai.ts:11`).
const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";

/// Where a device code, and later a refresh token, is exchanged for tokens
/// (`xai.ts:12`).
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";

/// What the login asks for (`xai.ts:22`).
///
/// `offline_access` is the load-bearing one: it is what makes the token
/// endpoint issue a refresh token at all, and without it every expiry would
/// cost another login.
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

/// How long an access token is assumed to live when the endpoint did not say.
///
/// Upstream's fallback, at both the places it computes an expiry
/// (`xai.ts:499`, `:574`, `:610`: `expires_in ?? 3600`).
const DEFAULT_EXPIRES_IN_S: u64 = 3_600;

/// The port the redirect is registered against (`xai.ts:37`).
///
/// **Not ours to choose, and not ours to fall back from.** Upstream's own
/// comment says why (`xai.ts:33-35`): "xAI rejects redirect_uris that don't
/// match what was registered for the Grok-CLI client. The host:port pair is
/// part of the registration, so we have to bind the loopback server to this
/// exact port." A login that quietly took a free port instead would be a login
/// the authorization endpoint refuses, minutes later, in front of a person —
/// so a port that cannot be bound is reported here with the two ways out.
pub const CALLBACK_PORT: u16 = 56121;

/// The host half of that registration (`xai.ts:36`), which is also the only
/// address [`loopback::Listener`] will bind.
const CALLBACK_HOST: &str = "127.0.0.1";

/// The path the redirect names (`xai.ts:38`).
const CALLBACK_PATH: &str = "/callback";

/// The consent-screen tier the loopback client is registered under
/// (`xai.ts:138`).
///
/// Upstream's own comment is the reason it is not optional (`xai.ts:124-126`):
/// "`plan=generic` opts the consent screen into xAI's generic OAuth plan tier;
/// without it, accounts.x.ai rejects loopback OAuth from non-allowlisted
/// clients." Dropping it is not a tidier URL, it is a login that fails at the
/// browser.
const PLAN: &str = "generic";

/// Who xAI is told the login came from (`xai.ts:139`).
///
/// **Deliberately still `opencode`**, for the reason
/// [`super::openai`]'s `ORIGINATOR` is: this is a parameter of somebody else's
/// client registration, alongside [`CLIENT_ID`] and [`CALLBACK_PORT`], and the
/// combination that was measured against the live endpoint is that project's.
/// Upstream describes it as best-effort attribution in xAI's OAuth server logs
/// (`xai.ts:126-128`).
const REFERRER: &str = "opencode";

/// How long a browser login waits for the callback (`xai.ts:430-437`).
pub const CALLBACK_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// One exchange's whole budget: connect, headers and body.
///
/// The same bound [`Refresh`] puts on a renewal, for the same reason: an
/// endpoint that answers instantly and then dribbles forever is otherwise
/// unbounded.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// The device login against xAI's real endpoints.
///
/// # Errors
///
/// Returns [`DeviceError::Unreachable`] when no HTTP client can be built.
pub fn device_flow() -> Result<DeviceFlow, DeviceError> {
    device_flow_at(DEVICE_AUTHORIZATION_URL, TOKEN_URL)
}

/// The same login against endpoints of the caller's choosing, which is how a
/// test drives it against a loopback socket.
///
/// # Errors
///
/// Returns [`DeviceError::Unreachable`] when no HTTP client can be built.
pub fn device_flow_at(
    device_code_url: impl Into<String>,
    token_url: impl Into<String>,
) -> Result<DeviceFlow, DeviceError> {
    DeviceFlow::new(
        device_code_url,
        token_url,
        CLIENT_ID,
        SCOPE,
        // RFC 8628 §3.1's encoding, which is what xAI's endpoints take
        // (`xai.ts:87-93`). GitHub's want JSON; see `super::copilot`.
        BodyEncoding::Form,
    )
}

/// A browser login could not be completed.
///
/// Separate from [`DeviceError`] because the two flows fail at different
/// things: only this one can fail to bind a socket, and only the device one has
/// a poll loop to expire. Folding them would give each flow variants it can
/// never produce, and a caller no way to tell which of them it is holding.
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// The registered callback port could not be listened on.
    ///
    /// Its own variant, and the message is the point. The port is fixed by
    /// somebody else's client registration, so the only two ways out are to
    /// free it or to use the other login — and a person staring at an
    /// `EADDRINUSE` has no way to know either.
    #[error(
        "the login could not listen on {CALLBACK_HOST}:{port} for the browser's callback \
         ({source}); xAI registered that exact address for this client, so no other port \
         will do — close whatever else is holding it (another ganja or grok-cli login), \
         or log in with the device method instead"
    )]
    PortTaken {
        /// The port that was asked for.
        port: u16,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The unguessable values a login is built on could not be drawn.
    #[error(transparent)]
    Entropy {
        /// What the platform's random source said.
        source: EntropyError,
    },
    /// The browser's callback did not arrive, or did not belong to this login.
    #[error(transparent)]
    Callback {
        /// What the listener said.
        source: LoopbackError,
    },
    /// No HTTP client could be built, which in practice means the TLS backend
    /// failed to initialize.
    #[error("no HTTP client for the xAI login: {source}")]
    Client {
        /// What the client builder said.
        #[source]
        source: reqwest::Error,
    },
    /// The token endpoint could not be reached.
    ///
    /// The cause has had its URL stripped, because `reqwest` renders the whole
    /// request URL in its own message and a redirected endpoint is somewhere a
    /// secret could have been configured.
    #[error("the xAI token endpoint could not be reached while exchanging the code: {source}")]
    Unreachable {
        /// What the transport said, without its URL.
        #[source]
        source: reqwest::Error,
    },
    /// The token endpoint answered, and refused.
    ///
    /// `code` is xAI's own OAuth error code where it gave one in the shape RFC
    /// 6749 §5.2 defines, and never a response body: the body of a refused
    /// exchange routinely quotes the request back, and the request held the
    /// authorization code and the verifier together.
    #[error("the xAI token endpoint refused the exchange: {reason}")]
    Refused {
        /// The status and, where there was one, the endpoint's own code.
        reason: String,
    },
    /// The exchange succeeded and carried no access token.
    #[error("the xAI token endpoint returned no access token")]
    Malformed,
    /// Nobody completed the authorization in time.
    #[error("the xAI login was not completed within {}s", .after.as_secs())]
    TimedOut {
        /// How long was allowed.
        after: Duration,
    },
    /// The login was cancelled.
    #[error("the xAI login was cancelled")]
    Cancelled,
}

impl From<EntropyError> for BrowserError {
    fn from(source: EntropyError) -> Self {
        Self::Entropy { source }
    }
}

impl From<LoopbackError> for BrowserError {
    /// Keeps one spelling per outcome.
    ///
    /// A login that ran out of time or was cancelled says so the same way
    /// whichever flow it was, and a port that could not be bound is lifted out
    /// of the listener's generic wording into the one that names the remedy.
    fn from(error: LoopbackError) -> Self {
        match error {
            LoopbackError::Bind { port, source } => Self::PortTaken { port, source },
            LoopbackError::TimedOut { after } => Self::TimedOut { after },
            LoopbackError::Cancelled => Self::Cancelled,
            source => Self::Callback { source },
        }
    }
}

/// The browser login against xAI's real endpoints.
///
/// # Errors
///
/// Returns [`BrowserError::Client`] when no HTTP client can be built.
pub fn browser_flow() -> Result<BrowserFlow, BrowserError> {
    browser_flow_at(AUTHORIZE_URL, TOKEN_URL)
}

/// The same login against endpoints of the caller's choosing, which is how a
/// test drives it against a loopback socket.
///
/// The pair mirrors [`device_flow_at`] deliberately: both flows are redirected
/// by the same seam, so a suite that owns one owns the other the same way.
///
/// # Errors
///
/// Returns [`BrowserError::Client`] when no HTTP client can be built.
pub fn browser_flow_at(
    authorize_url: impl Into<String>,
    token_url: impl Into<String>,
) -> Result<BrowserFlow, BrowserError> {
    let client =
        super::login_client(EXCHANGE_TIMEOUT).map_err(|source| BrowserError::Client { source })?;

    Ok(BrowserFlow {
        client,
        authorize_url: authorize_url.into(),
        token_url: token_url.into(),
    })
}

/// xAI's browser login: where to send somebody, and what to do with what comes
/// back.
///
/// No `Debug`, deliberately. The type is held for the length of a login, which
/// is exactly the window in which a `tracing` field would render it, and its
/// URLs are configuration — [`browser_flow_at`] takes whatever it is given, so
/// there is no check here that could promise a redirected endpoint carries no
/// userinfo. A type that cannot be formatted cannot leak one.
#[derive(Clone)]
pub struct BrowserFlow {
    /// What the exchange goes out on.
    client: reqwest::Client,
    /// Where the browser is sent.
    authorize_url: String,
    /// Where the code is spent.
    token_url: String,
}

impl BrowserFlow {
    /// Starts a login on the port xAI registered.
    ///
    /// # Errors
    ///
    /// Returns [`BrowserError::PortTaken`] when [`CALLBACK_PORT`] is already
    /// bound — which, the port being fixed, means another login is holding it —
    /// and [`BrowserError::Entropy`] when the platform has no entropy.
    pub async fn start(&self) -> Result<Browser, BrowserError> {
        self.start_on(CALLBACK_PORT).await
    }

    /// Starts a login on `port`.
    ///
    /// Port `0` takes whatever the operating system has free. xAI will only
    /// redirect to the registered port, so this is for a test standing up its
    /// own authorization endpoint rather than for production use.
    ///
    /// The socket is bound before the URL exists, which is both orders that
    /// matter: the redirect has to name the port that was actually got, and a
    /// browser opened before anything is listening can finish the login into a
    /// closed port.
    ///
    /// # Errors
    ///
    /// As [`start`](Self::start).
    pub async fn start_on(&self, port: u16) -> Result<Browser, BrowserError> {
        let listener = loopback::Listener::bind(port).await?;
        let pkce = Pkce::generate()?;
        let state = pkce::unguessable()?;
        // Published and never checked, which is upstream's behaviour too
        // (`xai.ts:558`, `:136`): xAI echoes it into an `id_token` this build
        // does not read. It is drawn per login all the same, because a `nonce`
        // shared between two logins is not a `nonce`.
        let nonce = pkce::unguessable()?;
        let redirect = format!("http://{CALLBACK_HOST}:{}{CALLBACK_PATH}", listener.port());
        let url = self.authorize_url(
            &redirect,
            pkce.challenge(),
            state.expose_secret(),
            nonce.expose_secret(),
        );

        Ok(Browser {
            flow: self.clone(),
            url,
            redirect,
            listener,
            pkce,
            state,
        })
    }

    /// Where the person is sent (`xai.ts:118-142`).
    ///
    /// The parameters are in upstream's own insertion order, which means
    /// nothing to xAI and a great deal to whoever next compares the two files.
    fn authorize_url(&self, redirect: &str, challenge: &str, state: &str, nonce: &str) -> String {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", redirect)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("plan", PLAN)
            .append_pair("referrer", REFERRER)
            .finish();

        format!("{}?{query}", self.authorize_url)
    }

    /// Trades an authorization code for tokens (`xai.ts:144-172`).
    ///
    /// `redirect_uri` is repeated here because RFC 6749 §4.1.3 requires the
    /// exchange to present the same one the authorization did; `code_verifier`
    /// is what proves the code was issued to this login and not intercepted
    /// from it.
    async fn exchange(
        &self,
        code: &SecretString,
        redirect: &str,
        verifier: &SecretString,
    ) -> Result<Tokens, BrowserError> {
        let sent = self
            .client
            .post(&self.token_url)
            // The same three headers upstream sends (`xai.ts:87-93`), through
            // the same encoder the device flow's bodies go through.
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, UPSTREAM_USER_AGENT)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form(&[
                ("grant_type", "authorization_code"),
                ("code", code.expose_secret()),
                ("redirect_uri", redirect),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier.expose_secret()),
            ]))
            .send()
            .await;

        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                return Err(BrowserError::Unreachable {
                    source: error.without_url(),
                });
            }
        };
        let status = response.status();
        // A body that will not read is a body with no error code in it, which
        // is exactly what an empty one means to `refusal` below.
        let fields = json_object(&response.text().await.unwrap_or_default());

        if !status.is_success() {
            // Carries the provider and the status and nothing else. Upstream
            // puts the whole body into its own error (`xai.ts:166-169`), and
            // that body is where a refused exchange quotes the code back.
            tracing::debug!(
                provider = PROVIDER_ID,
                status = status.as_u16(),
                "the token endpoint would not exchange the authorization code",
            );

            return Err(BrowserError::Refused {
                reason: refusal(status.as_u16(), &fields),
            });
        }

        let Some(access) = text(&fields, "access_token") else {
            return Err(BrowserError::Malformed);
        };

        Ok(Tokens {
            access: SecretString::from(access),
            refresh: text(&fields, "refresh_token").map(SecretString::from),
            expires_in: positive_seconds_ms(fields.get("expires_in")).map(|ms| ms / 1_000),
        })
    }
}

/// A browser login whose listener is bound and whose URL is ready to open.
pub struct Browser {
    /// The login this belongs to.
    flow: BrowserFlow,
    /// Where the person is sent.
    url: String,
    /// What xAI was told to redirect to, repeated at the exchange because RFC
    /// 6749 §4.1.3 requires the two to match.
    redirect: String,
    /// Where the redirect lands.
    listener: loopback::Listener,
    /// The proof that the code being exchanged was issued to this login.
    pkce: Pkce,
    /// The proof that the callback belongs to this login.
    state: SecretString,
}

impl fmt::Debug for Browser {
    /// Hand-written because [`url`](Self::url) carries the `state` in its query.
    ///
    /// A derived `Debug` would render it, and this type is held for the length
    /// of a login — exactly the window in which a `tracing` field, or somebody's
    /// `{:?}` in a diagnostic, would put the value that decides whose callback
    /// is accepted into a log. The port and the challenge are what identify a
    /// login for debugging, and neither is a secret: the challenge is published
    /// in that same URL, and the port is on the wire.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Browser")
            .field("port", &self.listener.port())
            .field("challenge", &self.pkce.challenge())
            .field("redirect", &self.redirect)
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
    /// [`CALLBACK_DEADLINE`] is upstream's bound and the one to pass unless
    /// there is a reason not to; `cancel` is the one that answers a keystroke.
    ///
    /// The `state` check inside [`loopback::Listener::wait`] happens **before**
    /// the redirect's `error` parameter is read, which is that module's
    /// deliberate divergence from upstream and is inherited here rather than
    /// re-decided: upstream's own handler reads `error` first (`xai.ts:332-341`)
    /// and only then compares `state` (`xai.ts:352`).
    ///
    /// # Errors
    ///
    /// Returns [`BrowserError`]: [`Callback`] when the redirect was refused,
    /// forged or empty, [`TimedOut`] or [`Cancelled`] when nobody finished, and
    /// the token endpoint's own failures from the exchange.
    ///
    /// [`Callback`]: BrowserError::Callback
    /// [`TimedOut`]: BrowserError::TimedOut
    /// [`Cancelled`]: BrowserError::Cancelled
    pub async fn wait(
        self,
        within: Duration,
        cancel: &CancellationToken,
    ) -> Result<Tokens, BrowserError> {
        let code = self
            .listener
            .wait(CALLBACK_PATH, &self.state, within, cancel)
            .await?;

        self.flow
            .exchange(&code, &self.redirect, self.pkce.verifier())
            .await
    }
}

/// The credential a completed login stores.
///
/// The expiry is computed the way every login flow in this build computes one
/// — [`now_ms`] plus the lifetime in milliseconds (`xai.ts:610`) — through the
/// shared helper rather than a second hand-rolled clock.
///
/// **Both flows land here**, which is what makes a browser login and a device
/// login store the same bytes under the same key rather than two shapes that
/// happen to look alike.
///
/// A grant that returned no refresh token leaves that field blank
/// rather than borrowing the access token for it. The credential still works
/// until it expires; what it cannot do is renew itself, and
/// [`crate::auth::RefreshOauth::refresh`] says exactly that rather than presenting an access
/// token to an endpoint that wants a refresh one.
#[must_use]
pub fn credential_from(tokens: &Tokens) -> OauthCredential {
    OauthCredential::new(
        tokens
            .refresh
            .clone()
            .unwrap_or_else(|| SecretString::from("")),
        tokens.access.clone(),
        expiry(tokens.expires_in),
    )
}

/// When a token issued now with a stated lifetime of `expires_in` seconds
/// stops being accepted.
fn expiry(expires_in: Option<u64>) -> u64 {
    now_ms().saturating_add(
        expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN_S)
            .saturating_mul(1_000),
    )
}

/// Renews an xAI credential from the refresh token stored beside it.
///
/// Spec: `xai.ts:167-182` for the request and `:498-517` for what is kept.
/// The refresh token **rotates**: the endpoint hands back a new one and
/// considers the old one spent, so the new one is what gets stored — falling
/// back to the old only when the endpoint returned none, which is upstream's
/// `tokens.refresh_token || refreshToken` (`:500`).
///
/// This type does no storing of its own. [`super::Refresher`] is what holds
/// the renewal to one at a time and writes the result down; this is only the
/// endpoint half, which is the split [`super::RefreshOauth`] exists to make.
pub struct Refresh {
    client: reqwest::Client,
    token_url: String,
}

impl Refresh {
    /// A refresher against xAI's real token endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::RefreshUnavailable`] when no HTTP client can be
    /// built — a TLS backend that will not initialise, which is transient in
    /// the sense that matters: nothing was refused.
    pub fn new() -> Result<Self, AuthError> {
        Self::at(TOKEN_URL)
    }

    /// The same, against an endpoint of the caller's choosing.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::RefreshUnavailable`] when no HTTP client can be
    /// built.
    pub fn at(token_url: impl Into<String>) -> Result<Self, AuthError> {
        let client = super::login_client(std::time::Duration::from_secs(30)).map_err(|error| {
            AuthError::RefreshUnavailable {
                provider_id: PROVIDER_ID.to_owned(),
                reason: error.without_url().to_string(),
            }
        })?;

        Ok(Self {
            client,
            token_url: token_url.into(),
        })
    }
}

#[async_trait::async_trait]
impl super::RefreshOauth for Refresh {
    /// Trades `credential`'s refresh token for a fresh pair.
    ///
    /// The classification is the whole point of this method, and getting it
    /// backwards is a real defect rather than a wording choice:
    ///
    /// - The endpoint **refused** the token — a 4xx — means the credential is
    ///   dead and only a new login fixes it, so
    ///   [`AuthError::ReauthRequired`].
    /// - The attempt **never got there** — connection refused, DNS, a
    ///   timeout — or the endpoint itself broke — a 5xx — means the stored
    ///   credential is untouched and retrying is the answer, so
    ///   [`AuthError::RefreshUnavailable`].
    ///
    /// Folding the second into the first sends someone whose network dropped
    /// through a browser login they did not need; folding the first into the
    /// second retries a dead token forever.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::ReauthRequired`] or
    /// [`AuthError::RefreshUnavailable`], as above. Neither `reason` ever
    /// carries a response body or a token: it is a status and, when the
    /// endpoint named one in the shape an error code has, that code.
    async fn refresh(
        &self,
        provider_id: &str,
        credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        let stored = credential.refresh.expose_secret();
        if stored.trim().is_empty() {
            // Nothing to present. Saying so beats a round trip that can only
            // come back as the same answer, and it is a genuinely different
            // situation from a token the endpoint rejected.
            return Err(AuthError::ReauthRequired {
                provider_id: provider_id.to_owned(),
                reason: "no refresh token is stored beside it".to_owned(),
            });
        }

        let sent = self
            .client
            .post(&self.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, UPSTREAM_USER_AGENT)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", stored),
                ("client_id", CLIENT_ID),
            ]))
            .send()
            .await;

        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                return Err(AuthError::RefreshUnavailable {
                    provider_id: provider_id.to_owned(),
                    reason: error.without_url().to_string(),
                });
            }
        };
        let status = response.status();
        // A body that will not read is a body with no error code in it, which
        // is exactly what an empty one means to `reason` below.
        let fields = json_object(&response.text().await.unwrap_or_default());

        if !status.is_success() {
            let reason = refusal(status.as_u16(), &fields);
            // Carries the provider and the status and nothing else. The body
            // this came from holds a refresh token whenever the endpoint
            // echoes the request back, which some do.
            tracing::debug!(
                provider = %provider_id,
                status = status.as_u16(),
                "the token endpoint would not renew the stored credential",
            );

            return Err(if status.is_client_error() {
                AuthError::ReauthRequired {
                    provider_id: provider_id.to_owned(),
                    reason,
                }
            } else {
                // A 5xx is the endpoint being broken, not the token being
                // dead. Upstream draws no such line (`xai.ts:177-180` throws
                // one error for every non-`ok`), which leaves it unable to
                // tell a person whether logging in again would help.
                AuthError::RefreshUnavailable {
                    provider_id: provider_id.to_owned(),
                    reason,
                }
            });
        }

        let Some(access) = text(&fields, "access_token") else {
            return Err(AuthError::ReauthRequired {
                provider_id: provider_id.to_owned(),
                reason: "the token endpoint returned no access token".to_owned(),
            });
        };

        Ok(OauthCredential::new(
            // Rotated, with the old one kept when the endpoint sent no new one
            // (`xai.ts:500`). The old one is already spent if it did rotate,
            // so guessing wrong in this direction costs a login.
            text(&fields, "refresh_token")
                .map_or_else(|| credential.refresh.clone(), SecretString::from),
            SecretString::from(access),
            expiry(positive_seconds_ms(fields.get("expires_in")).map(|ms| ms / 1_000)),
        ))
    }
}

/// Why the token endpoint refused, in a form that can be shown and logged.
///
/// A status, plus the endpoint's own error code when what arrived looked like
/// one. Never the body: an OAuth token endpoint's error body routinely quotes
/// the request back, and every request this file makes to it holds either a
/// refresh token or an authorization code and the verifier that spends it.
/// Shared by the renewal and the browser flow's exchange so that neither can
/// grow its own idea of how much of a refusal may be repeated.
fn refusal(status: u16, fields: &Map<String, Value>) -> String {
    fields.get("error").and_then(Value::as_str).map_or_else(
        || format!("HTTP {status}"),
        |code| format!("HTTP {status}, {}", reportable_code(code)),
    )
}

#[cfg(test)]
mod tests {
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
        BrowserError, CALLBACK_PORT, CLIENT_ID, PROVIDER_ID, Refresh, SCOPE, UPSTREAM_USER_AGENT,
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
        // Upstream's own product name, against upstream's own registered
        // client id — the combination the live spikes measured. Asserted as a
        // literal as well as through the constant, so that changing the
        // constant is a decision somebody has to come here and confirm.
        assert!(request.has_header("user-agent", UPSTREAM_USER_AGENT));
        assert!(request.has_header("user-agent", "opencode/1.18.22"));

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
            let refresher =
                Refresh::at(format!("{}/token", endpoint.url)).expect("a client builds");

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
                ("referrer".to_owned(), "opencode".to_owned()),
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
        assert!(sent.has_header("user-agent", UPSTREAM_USER_AGENT));

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
}
