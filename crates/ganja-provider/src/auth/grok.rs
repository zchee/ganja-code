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

use std::time::Duration;
use std::{fmt, io};

use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

use super::device::{
    BodyEncoding, DeviceError, DeviceFlow, Tokens, form, json_object, positive_seconds_ms,
    reportable_code, text,
};
use super::loopback::{self, LoopbackError};
use super::pkce::{self, EntropyError, Pkce};
use super::{AuthError, OauthCredential, now_ms};

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
/// **ganja's own name**, moved in W4 of
/// `.omc/plans/2026-08-25-ganja-code-identity-headers.md`. It is a parameter
/// of somebody else's client registration, alongside [`CLIENT_ID`] and
/// [`CALLBACK_PORT`] — but upstream describes it as best-effort attribution
/// in xAI's OAuth server logs (`xai.ts:126-128`), which is to say a field
/// whose entire purpose is to record who was asking. Carrying another
/// project's name in it was the one thing it could get wrong.
///
/// Moves with [`XAI_USER_AGENT`] and never alone.
const REFERRER: &str = "ganja-code";

/// What x.ai is told this build is.
///
/// [`GANJA_USER_AGENT`](super::device::GANJA_USER_AGENT)'s bytes, named here
/// rather than reached for directly so that **all three** of this host's call
/// sites — the device authorization, the browser code exchange and the
/// refresh — say one thing. A host that receives two different names from one
/// client is the mismatched telemetry signature abuse detection looks for,
/// which is why this and [`REFERRER`] beside it move together or not at all.
///
/// Moved in W4 of `.omc/plans/2026-08-25-ganja-code-identity-headers.md`,
/// after a real `ganja auth login grok` and a turn under it both completed.
/// There is no seat roster on this host to diff as there is on the codex
/// backend — x.ai's models come from the catalog rather than from a per-seat
/// ladder — so the gate was the login and the turn themselves, recorded in
/// `crates/ganja-core/tests/fixtures/grok-identity-probe.txt`. That file is
/// composed by hand from that login and turn, in the shape P27/P28's
/// `*-probe.txt` fixtures use, and re-recorded by hand if the login is run
/// again: it is a record of what happened rather than a baseline anything is
/// diffed against.
pub(in crate::auth) const XAI_USER_AGENT: &str = super::device::GANJA_USER_AGENT;

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
        // This host's name, not the shared one: `XAI_USER_AGENT` is what keeps
        // the device authorization saying the same thing as the exchange and
        // the refresh below it.
        XAI_USER_AGENT,
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

    Ok(BrowserFlow { client, authorize_url: authorize_url.into(), token_url: token_url.into() })
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

        Ok(Browser { flow: self.clone(), url, redirect, listener, pkce, state })
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
        let sent = token_request(&self.client, &self.token_url)
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
                return Err(BrowserError::Unreachable { source: error.without_url() });
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

            return Err(BrowserError::Refused { reason: refusal(status.as_u16(), &fields) });
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
        let code = self.listener.wait(CALLBACK_PATH, &self.state, within, cancel).await?;

        self.flow.exchange(&code, &self.redirect, self.pkce.verifier()).await
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
        tokens.refresh.clone().unwrap_or_else(|| SecretString::from("")),
        tokens.access.clone(),
        expiry(tokens.expires_in),
    )
}

/// When a token issued now with a stated lifetime of `expires_in` seconds
/// stops being accepted.
fn expiry(expires_in: Option<u64>) -> u64 {
    now_ms().saturating_add(expires_in.unwrap_or(DEFAULT_EXPIRES_IN_S).saturating_mul(1_000))
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

        Ok(Self { client, token_url: token_url.into() })
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

        let sent = token_request(&self.client, &self.token_url)
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
                AuthError::ReauthRequired { provider_id: provider_id.to_owned(), reason }
            } else {
                // A 5xx is the endpoint being broken, not the token being
                // dead. Upstream draws no such line (`xai.ts:177-180` throws
                // one error for every non-`ok`), which leaves it unable to
                // tell a person whether logging in again would help.
                AuthError::RefreshUnavailable { provider_id: provider_id.to_owned(), reason }
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

/// The POST every grant to the token endpoint rides — the same three headers
/// upstream sends (`xai.ts:87-93`), with the body left to each caller through
/// the same encoder the device flow's bodies go through. Shared by the
/// renewal and the browser flow's exchange so the two cannot drift over what
/// this endpoint is told; what each does with the *answer* stays its own,
/// because their error classifications are load-bearing and different.
fn token_request(client: &reqwest::Client, token_url: &str) -> reqwest::RequestBuilder {
    client
        .post(token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, XAI_USER_AGENT)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
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
#[path = "grok_tests.rs"]
mod tests;
