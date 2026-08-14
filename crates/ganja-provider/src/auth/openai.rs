//! Signing in to ChatGPT, and keeping that credential alive.
//!
//! Spec: upstream `packages/core/src/plugin/provider/openai.ts` throughout,
//! cross-checked against `packages/opencode/src/plugin/openai/codex.ts`, which
//! agrees on every endpoint and spells the loopback handler and the account-id
//! extraction out at `:154-225` and `:38-73`.
//!
//! **What this delivers, and what it does not.** Two ways in — a browser login
//! through a loopback redirect, and a device login for a machine with no
//! browser — plus the renewal that keeps the result usable. What it does *not*
//! deliver is working ChatGPT models: a subscription talks to the Responses
//! API rather than to chat completions (`openai.ts:182-187`, and the catalog
//! hook at `:161-174` disabling a chat-completions-only alias with that reason
//! written on it), and ganja's OpenAI provider speaks chat completions. **The
//! credential this stores therefore has no consumer yet.** It lands now with
//! its tests because it rotates and expires, and a stored rotating credential
//! that nothing can renew is a trap rather than a head start.
//!
//! **One hazard, recorded rather than solved.** This stores under provider id
//! `openai`, which is the same key an OpenAI *API key* is stored under, so a
//! ChatGPT login replaces a stored API key and storing an API key replaces a
//! ChatGPT login. That is upstream's behaviour at this key too. Warning
//! somebody before they lose a key belongs to whatever runs the login, which is
//! not this module; a test pins the behaviour so that it is a decision on the
//! record rather than a surprise.
//!
//! Two things about this file are worth knowing before reading it:
//!
//! - **The device flow here is not RFC 8628.** It is ChatGPT's own: the codes
//!   come from `/api/accounts/deviceauth/*`, the pending signal is an HTTP
//!   status rather than an error body, and the server — not the client — mints
//!   the PKCE verifier. Each of those is called out where it happens.
//! - **Nothing here writes to the credential store.** Every path returns a
//!   credential or an error, and storing it is the caller's step. Together with
//!   the same property in [`loopback`], that makes "a login
//!   that failed stores nothing" a fact about which functions exist rather than
//!   a claim about which branches were taken.

use std::{fmt, time::Duration};

use base64::{
    Engine as _, alphabet,
    engine::{GeneralPurpose, general_purpose::NO_PAD_INDIFFERENT},
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::{Url, form_urlencoded};

use super::{
    AuthError, OauthCredential, RedactedTail, RefreshOauth,
    loopback::{self, LoopbackError},
    now_ms,
    pkce::{self, EntropyError, Pkce},
};

/// The provider this logs in to, as ganja and `auth.json` both name it.
///
/// The single source of the string: a caller wanting it writes
/// `auth::openai::PROVIDER_ID` rather than a literal, so a rename is one edit.
pub const PROVIDER_ID: &str = "openai";

/// The public client the ChatGPT CLI flows are registered as (`openai.ts:14`).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Where the authorization lives (`openai.ts:15`).
const ISSUER: &str = "https://auth.openai.com";

/// The port the redirect is registered against (`openai.ts:16`).
///
/// Not ours to choose: it is part of the client registration above, so a login
/// on any other port is a login the issuer refuses.
pub const CALLBACK_PORT: u16 = 1455;

/// The path the redirect names (`openai.ts:51`).
const CALLBACK_PATH: &str = "/auth/callback";

/// What the login asks for (`openai.ts:83`).
///
/// `offline_access` is the load-bearing one: without it there is no refresh
/// token and the credential dies in an hour.
const SCOPE: &str = "openid profile email offline_access";

/// Who the issuer is told is asking (`openai.ts:271`).
///
/// **Deliberately still `opencode`.** It is not a User-Agent — it is a
/// parameter of a client registration that belongs to upstream, alongside the
/// client id and the callback port above, and a value the registration has
/// never been sent is a rejection nothing here could test for. What ganja does
/// say in its own name is the `User-Agent`, below.
const ORIGINATOR: &str = "opencode";

/// How this build identifies itself to the issuer.
///
/// Upstream's own string, for the same reason [`ORIGINATOR`] is: every one of
/// these logins presents a client id its own project registered, and the only
/// header shape ever measured against a live endpoint was that project's. A
/// `User-Agent` naming ganja, sent against somebody else's registered client,
/// is a combination nothing has tested — and an authorization endpoint is the
/// wrong place to discover it does not work.
///
/// The cost is real and worth naming: a server attributing traffic by this
/// header will credit ganja's requests to upstream. That is a choice, recorded
/// where it is made, and it is one constant across all three logins so it can
/// be revisited in one place.
const USER_AGENT: &str = crate::auth::device::UPSTREAM_USER_AGENT;

/// What a form-encoded body is announced as.
const FORM: &str = "application/x-www-form-urlencoded";

/// Added to the provider's polling interval (`openai.ts:17`, `:146`).
const POLL_MARGIN: Duration = Duration::from_secs(3);

/// Seconds between device polls when the provider named none (`openai.ts:113`).
const DEFAULT_POLL_SECONDS: u64 = 5;

/// How long an access token lasts when the issuer did not say (`openai.ts:244`).
const DEFAULT_EXPIRES_IN: u64 = 3600;

/// One request's whole budget: connect, headers and body.
///
/// An endpoint that answers instantly and then dribbles forever is otherwise
/// unbounded — the same reasoning [`crate::catalog`]'s fetch is built on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a browser login waits for the callback (`codex.ts:238-246`).
pub const CALLBACK_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// How long a device login polls before giving up.
///
/// **Ganja's own**: upstream's loop has no deadline at all
/// (`openai.ts:119-147`, a `while (true)`), which is survivable only because
/// the person can `Ctrl-C` the CLI it runs in. A login driven from anywhere
/// else — a TUI, a future `ganja serve` — has no such key to press, so the
/// bound is in the code. Fifteen minutes is long enough to pick up another
/// device and type an eight-character code.
pub const DEVICE_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// Stands in for a refusal the issuer gave no usable code for.
const UNNAMED: &str = "no error code given";

/// Naming the step a failure happened at, for the message a person reads.
mod step {
    /// Asking for a device code.
    pub(super) const STARTING: &str = "starting the device authorization";

    /// Waiting for the device code to be entered.
    pub(super) const POLLING: &str = "waiting for the device authorization";

    /// Trading the authorization code for tokens.
    pub(super) const EXCHANGING: &str = "exchanging the authorization code";

    /// Spending the refresh token.
    pub(super) const RENEWING: &str = "renewing the credential";
}

/// The engine an `id_token` payload is decoded with.
///
/// base64url, which JWS mandates (RFC 7515 §2), but **indifferent about
/// padding**. RFC 7515 §2 says the segments carry no `=` and producers mostly
/// agree; one that pads is producing a token this build already does not trust,
/// so refusing it would be strictness bought with a login that fails for no
/// security reason. All this decode feeds is a routing hint.
const CLAIMS: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, NO_PAD_INDIFFERENT);

/// A ChatGPT login could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The issuer is not somewhere tokens may travel to.
    ///
    /// The URL is deliberately absent from the message: an issuer is
    /// configuration, and configuration is allowed to carry credentials in its
    /// userinfo — the same reasoning [`crate::provider`]'s base-URL check is
    /// written with.
    #[error(
        "the ChatGPT issuer must be https, or http to loopback, and must carry \
         no user, password, query or fragment; anything else either puts the \
         login's tokens on the wire in the clear or puts a secret into the URL \
         a person is shown"
    )]
    Issuer,
    /// No HTTP client could be built, which in practice means the TLS backend
    /// failed to initialize.
    #[error("no HTTP client for the ChatGPT login: {source}")]
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
    /// The browser's callback did not arrive, or did not belong to this login.
    #[error(transparent)]
    Callback {
        /// What the listener said.
        source: LoopbackError,
    },
    /// The issuer could not be reached.
    ///
    /// The cause has had its URL stripped: `reqwest` renders the whole request
    /// URL in its own message, and a device poll's URL is not somewhere a
    /// secret can hide but an issuer someone configured is.
    #[error("the ChatGPT issuer could not be reached while {step}: {source}")]
    Unreachable {
        /// What was being attempted.
        step: &'static str,
        /// What the transport said, without its URL.
        #[source]
        source: reqwest::Error,
    },
    /// The issuer answered, and refused.
    ///
    /// `code` is the issuer's own OAuth error code or `UNNAMED` — never a
    /// response body, which is a place a token can appear.
    #[error("the ChatGPT issuer refused while {step}: HTTP {status} ({code})")]
    Refused {
        /// What was being attempted.
        step: &'static str,
        /// The status it answered with.
        status: u16,
        /// Its own name for the refusal.
        code: String,
    },
    /// The issuer answered with something this cannot use.
    ///
    /// The decoder's own message is thrown away, because `serde_json` quotes
    /// the offending value back and every value in these answers is a token.
    /// The same reasoning as [`AuthError::Malformed`].
    #[error("the ChatGPT issuer's answer while {step} was not the shape a login expects")]
    Malformed {
        /// What was being attempted.
        step: &'static str,
    },
    /// Nobody completed the authorization in time.
    #[error("the ChatGPT login was not completed within {}s", .after.as_secs())]
    TimedOut {
        /// How long was allowed.
        after: Duration,
    },
    /// The login was cancelled.
    #[error("the ChatGPT login was cancelled")]
    Cancelled,
}

impl From<EntropyError> for LoginError {
    fn from(source: EntropyError) -> Self {
        Self::Entropy { source }
    }
}

impl From<LoopbackError> for LoginError {
    /// Keeps one spelling per outcome.
    ///
    /// A login that ran out of time or was cancelled says so the same way
    /// whichever flow it was, so a caller branching on those does not have to
    /// know that one of them owns a socket and the other does not.
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
    ///
    /// The whole distinction: [`AuthError::ReauthRequired`] means the grant is
    /// gone and only a browser fixes it, [`AuthError::RefreshUnavailable`]
    /// means the stored credential is still good and the attempt simply did not
    /// land. Folding one into the other is a real defect in either direction —
    /// one sends someone through a browser flow to fix a dropped connection,
    /// the other leaves them retrying a credential that will never work again.
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
            Self::Refused { status, code, .. } => format!("HTTP {status}, {code}"),
            Self::Unreachable { source, .. } => format!("the issuer was not reachable: {source}"),
            Self::Malformed { .. } => {
                "the issuer's answer was not the shape a renewal has".to_owned()
            }
            Self::Issuer => "the issuer is not somewhere tokens may be sent".to_owned(),
            Self::Client { source } => format!("no HTTP client: {source}"),
            Self::Entropy { source } => source.to_string(),
            Self::Callback { source } => source.to_string(),
            Self::TimedOut { after } => format!("it did not answer within {}s", after.as_secs()),
            Self::Cancelled => "it was cancelled".to_owned(),
        }
    }
}

/// Whether a status from the token endpoint means the grant itself is gone.
///
/// A 4xx there is the issuer saying it will not honour this refresh token again
/// (RFC 6749 §5.2's `invalid_grant`), and only a new login fixes that.
///
/// **Except `429`**, which says the opposite: come back later. Reading a rate
/// limit as a dead grant would send somebody through a browser flow to fix a
/// queue — precisely the confusion [`AuthError::RefreshUnavailable`] was added
/// to prevent. A `5xx` is the issuer's problem and not the credential's, so it
/// is not a dead grant either.
fn is_dead_grant(status: u16) -> bool {
    (400..500).contains(&status) && status != 429
}

/// A ChatGPT login, and the renewal for what it produces.
///
/// Cloneable because the flow objects each carry one and a `reqwest::Client` is
/// a handle to a shared pool rather than a connection.
#[derive(Clone, Debug)]
pub struct Login {
    /// Where the authorization lives, without a trailing slash.
    issuer: String,
    /// What every request goes out on.
    client: reqwest::Client,
}

impl Login {
    /// A login against ChatGPT's own issuer.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Client`] when no HTTP client can be built.
    pub fn new() -> Result<Self, LoginError> {
        Self::with_issuer(ISSUER)
    }

    /// A login against `issuer`.
    ///
    /// Upstream carries the same override for the same reason
    /// (`codex.ts:101-105`, `CodexAuthPluginOptions.issuer`): the flow is worth
    /// exercising end to end against something that is not the real ChatGPT.
    ///
    /// Redirects are refused outright. Every request here carries either a
    /// credential or the material to mint one, and `reqwest` only strips the
    /// headers it knows are credentials — a form body it would hand to whatever
    /// host a `3xx` names. Same reasoning as [`crate::provider`]'s client.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Issuer`] when `issuer` is not `https` or loopback,
    /// or carries anything a secret can hide in, and [`LoginError::Client`]
    /// when no HTTP client can be built.
    pub fn with_issuer(issuer: &str) -> Result<Self, LoginError> {
        let issuer = issuer.trim_end_matches('/').to_owned();
        let parsed = Url::parse(&issuer).map_err(|_| LoginError::Issuer)?;
        if !crate::provider::reachable_in_the_clear(&parsed) {
            return Err(LoginError::Issuer);
        }

        // An issuer is an origin and at most a path. Userinfo, a query or a
        // fragment in one means nothing to any OAuth endpoint, and each is
        // somewhere a secret gets put — so refusing them is not pedantry, it is
        // what keeps two things true elsewhere in this file. The authorize URL
        // is built by *prefixing* this string, and that URL is printed to a
        // terminal and handed to a browser; and `reqwest` turns userinfo into a
        // `Basic` header on every request, which would put it on the wire to
        // the token endpoint as well. Refusing at the door is also what makes
        // this type's derived `Debug` safe, rather than a redaction that has to
        // be remembered in three places. [`crate::provider`] treats userinfo on
        // a *base URL* as legitimate configuration — a gateway really does put
        // a token there — and redacts on the way out; an issuer has no such use.
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(LoginError::Issuer);
        }

        let client =
            super::login_client(REQUEST_TIMEOUT).map_err(|source| LoginError::Client { source })?;

        Ok(Self { issuer, client })
    }

    /// Starts a browser login on the port the issuer expects.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Callback`] when the port cannot be listened on —
    /// which, the port being fixed, usually means another login is already
    /// waiting — and [`LoginError::Entropy`] when the platform has no entropy.
    pub async fn browser(&self) -> Result<Browser, LoginError> {
        self.browser_on(CALLBACK_PORT).await
    }

    /// Starts a browser login on `port`.
    ///
    /// Port `0` takes whatever the operating system has free. The issuer will
    /// only redirect to the registered port, so this is for a test standing up
    /// its own issuer rather than for production use.
    ///
    /// The socket is bound before the URL exists, which is both orders that
    /// matter: the redirect has to name the port that was actually got, and a
    /// browser opened before anything is listening can finish the login into a
    /// closed port.
    ///
    /// # Errors
    ///
    /// As [`browser`](Self::browser).
    pub async fn browser_on(&self, port: u16) -> Result<Browser, LoginError> {
        let listener = loopback::Listener::bind(port).await?;
        let pkce = Pkce::generate()?;
        let state = pkce::unguessable()?;
        let redirect = format!("http://localhost:{}{CALLBACK_PATH}", listener.port());
        let url = self.authorize_url(&redirect, pkce.challenge(), state.expose_secret());

        Ok(Browser {
            login: self.clone(),
            url,
            redirect,
            listener,
            pkce,
            state,
        })
    }

    /// Asks for a device code, so the browser can be on another machine.
    ///
    /// Two steps, and it has to be: whoever is driving this must print the code
    /// before anything blocks on somebody typing it.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Unreachable`], [`LoginError::Refused`] or
    /// [`LoginError::Malformed`] as the issuer earns them.
    pub async fn device(&self) -> Result<Device, LoginError> {
        let answer = self
            .send(
                self.client
                    .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
                    .json(&json!({ "client_id": CLIENT_ID })),
                step::STARTING,
            )
            .await?;
        let started: DeviceCode = answer.into_json(step::STARTING)?;

        Ok(Device {
            url: format!("{}/codex/device", self.issuer),
            interval: poll_interval(started.interval.as_ref()),
            user_code: started.user_code,
            device_auth_id: started.device_auth_id,
            login: self.clone(),
        })
    }

    /// Where the person is sent (`openai.ts:260-273`).
    ///
    /// The parameters are in upstream's own insertion order, which is not
    /// meaningful to the issuer and is very meaningful to whoever next compares
    /// the two files.
    fn authorize_url(&self, redirect: &str, challenge: &str, state: &str) -> String {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", redirect)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", state)
            .append_pair("originator", ORIGINATOR)
            .finish();

        format!("{}/oauth/authorize?{query}", self.issuer)
    }

    /// Trades an authorization code for tokens (`openai.ts:195-207`).
    ///
    /// `verifier` rather than a [`Pkce`] because the two flows differ in where
    /// it came from: the browser flow presents the one it minted, and the device
    /// flow presents the one the *server* minted. Neither presents a challenge —
    /// that was spent at the authorize step — so a pair would be a field that is
    /// meaningless on one of the two paths.
    async fn exchange(
        &self,
        code: &SecretString,
        redirect: &str,
        verifier: &SecretString,
    ) -> Result<Tokens, LoginError> {
        let answer = self
            .send(
                self.form_post(
                    &format!("{}/oauth/token", self.issuer),
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", code.expose_secret()),
                        ("redirect_uri", redirect),
                        ("client_id", CLIENT_ID),
                        ("code_verifier", verifier.expose_secret()),
                    ],
                ),
                step::EXCHANGING,
            )
            .await?;

        answer.into_json(step::EXCHANGING)
    }

    /// Spends the refresh token for a fresh pair (`openai.ts:209-224`).
    async fn renew(&self, previous: &OauthCredential) -> Result<OauthCredential, LoginError> {
        let answer = self
            .send(
                self.form_post(
                    &format!("{}/oauth/token", self.issuer),
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", previous.refresh.expose_secret()),
                        ("client_id", CLIENT_ID),
                    ],
                ),
                step::RENEWING,
            )
            .await?;
        let tokens: Tokens = answer.into_json(step::RENEWING)?;

        // A renewal that did not rotate the refresh token has not revoked the
        // one that was sent: keeping it is what lets the *next* renewal happen
        // at all. ChatGPT's does rotate, which is why this credential is one of
        // the reasons `Refresher`'s single flight exists.
        let refresh = tokens
            .refresh_token
            .clone()
            .unwrap_or_else(|| previous.refresh.clone());

        // `Refresher::usable` carries the unmodelled fields forward too, so
        // this is belt and braces — but a caller renewing through this trait
        // directly would otherwise drop the account id, and upstream carries it
        // across a renewal explicitly (`openai.ts:221`).
        Ok(credential(tokens, refresh).inheriting(previous))
    }

    /// A form-encoded `POST`, the way upstream builds one.
    ///
    /// `URLSearchParams(…).toString()` upstream (`openai.ts:199-205`);
    /// [`super::device::form`] here — one encoder for every login body this
    /// build posts.
    fn form_post(&self, url: &str, pairs: &[(&str, &str)]) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, FORM)
            .body(super::device::form(pairs))
    }

    /// Sends one request and reads its answer.
    ///
    /// The status is deliberately *not* judged here. A device poll's "keep
    /// waiting" is an HTTP status (`openai.ts:143`), so a helper that turned
    /// every non-2xx into an error would make that loop read its own control
    /// flow back out of an error type. What this does own is the transport
    /// channel: a failure to reach the issuer at all is [`Unreachable`] and can
    /// be nothing else, which is what the renewal's classification rests on.
    ///
    /// [`Unreachable`]: LoginError::Unreachable
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        step: &'static str,
    ) -> Result<Answer, LoginError> {
        let unreachable = |source: reqwest::Error| LoginError::Unreachable {
            step,
            source: source.without_url(),
        };

        let response = request
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(unreachable)?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(unreachable)?;

        Ok(Answer { status, body })
    }
}

#[async_trait::async_trait]
impl RefreshOauth for Login {
    /// Renews a stored ChatGPT credential.
    ///
    /// The production caller for this arrives with the deferred Responses
    /// provider; it lands now because the credential rotates and expires, and
    /// pinning the classification is only possible while the endpoint that
    /// produces each class is in front of us.
    async fn refresh(
        &self,
        provider_id: &str,
        credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        self.renew(credential)
            .await
            .map_err(|error| error.into_auth(provider_id))
    }
}

/// A browser login whose listener is bound and whose URL is ready to open.
pub struct Browser {
    /// The login this belongs to.
    login: Login,
    /// Where the person is sent.
    url: String,
    /// What the issuer was told to redirect to, repeated at the exchange
    /// because RFC 6749 §4.1.3 requires the two to match.
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
    /// of a login — exactly the window in which a `tracing` field holding one,
    /// or a `{:?}` in somebody's diagnostic, would put the value that decides
    /// whose callback gets accepted into a log. The port and the challenge are
    /// what identify a login for debugging, and neither is a secret: the
    /// challenge is published in that same URL, and the port is on the wire.
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
    /// there is a reason not to. `cancel` is the one that answers a keystroke.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError`]: [`Callback`] when the redirect was refused,
    /// forged or empty, [`TimedOut`] or [`Cancelled`] when nobody finished, and
    /// the issuer's own failures from the exchange.
    ///
    /// [`Callback`]: LoginError::Callback
    /// [`TimedOut`]: LoginError::TimedOut
    /// [`Cancelled`]: LoginError::Cancelled
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
            .exchange(&code, &self.redirect, self.pkce.verifier())
            .await?;

        first_credential(tokens, step::EXCHANGING)
    }
}

/// A device login: a code to type, and somewhere to type it.
pub struct Device {
    /// The login this belongs to.
    login: Login,
    /// Where the person has to go, on whatever device has a browser.
    url: String,
    /// What they have to type there.
    user_code: String,
    /// What identifies this login to the polling endpoint.
    device_auth_id: String,
    /// How long the issuer asked to be left between polls.
    interval: Duration,
}

impl fmt::Debug for Device {
    /// Hand-written because `device_auth_id` is half a
    /// credential.
    ///
    /// It and `user_code` are together what `poll` presents to claim the grant,
    /// and unlike `user_code` — which exists to be read off a screen — the
    /// device id is never shown to anybody. Rendering it during the
    /// authorization window would put a pair into a log that could be replayed
    /// into somebody's tokens, so only its tail is shown.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Device")
            .field("url", &self.url)
            .field("user_code", &self.user_code)
            .field("device_auth_id", &RedactedTail::of(&self.device_auth_id))
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// Where the person has to go (`openai.ts:116`).
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// What they have to type when they get there.
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Polls until the code is entered.
    ///
    /// [`DEVICE_DEADLINE`] is the bound to pass unless there is a reason not
    /// to; see its own note on why there is one at all.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError`]: [`TimedOut`] or [`Cancelled`] when nobody
    /// finished, and the issuer's own failures otherwise.
    ///
    /// [`TimedOut`]: LoginError::TimedOut
    /// [`Cancelled`]: LoginError::Cancelled
    pub async fn wait(
        self,
        within: Duration,
        cancel: &CancellationToken,
    ) -> Result<OauthCredential, LoginError> {
        tokio::select! {
            () = cancel.cancelled() => Err(LoginError::Cancelled),
            polled = tokio::time::timeout(within, self.poll()) => {
                polled.unwrap_or(Err(LoginError::TimedOut { after: within }))
            }
        }
    }

    /// The poll loop (`openai.ts:118-148`).
    ///
    /// **`403` and `404` mean "keep waiting".** The pending signal on this flow
    /// is an HTTP status and not RFC 8628's `authorization_pending` error body
    /// (`openai.ts:143`), so there is no error code to read and no `slow_down`
    /// to back off for — the interval the issuer named is the whole cadence.
    async fn poll(&self) -> Result<OauthCredential, LoginError> {
        loop {
            let answer = self
                .login
                .send(
                    self.login
                        .client
                        .post(format!(
                            "{}/api/accounts/deviceauth/token",
                            self.login.issuer
                        ))
                        .json(&json!({
                            "device_auth_id": self.device_auth_id,
                            "user_code": self.user_code,
                        })),
                    step::POLLING,
                )
                .await?;

            if answer.accepted() {
                let granted: DeviceGrant = answer.into_json(step::POLLING)?;

                // **The server minted the PKCE secret on this path**
                // (`openai.ts:131-141`): the 200 carries both the code and the
                // verifier that proves it, and a verifier generated here would
                // simply be the wrong one. Measured against the live endpoint
                // before it was written down.
                let tokens = self
                    .login
                    .exchange(
                        &granted.authorization_code,
                        &format!("{}/deviceauth/callback", self.login.issuer),
                        &granted.code_verifier,
                    )
                    .await?;

                return first_credential(tokens, step::EXCHANGING);
            }

            if !answer.pending() {
                return Err(answer.refused(step::POLLING));
            }

            tokio::time::sleep(self.interval + POLL_MARGIN).await;
        }
    }
}

/// A response, before anything has judged it.
struct Answer {
    /// What it answered with.
    status: u16,
    /// What it said, still undecoded.
    body: String,
}

impl Answer {
    /// Whether this is the answer a flow was waiting for.
    fn accepted(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Whether this is the device endpoint saying "not yet" (`openai.ts:143`).
    fn pending(&self) -> bool {
        self.status == 403 || self.status == 404
    }

    /// This answer decoded, or why it could not be used.
    fn into_json<T: serde::de::DeserializeOwned>(
        self,
        step: &'static str,
    ) -> Result<T, LoginError> {
        if !self.accepted() {
            return Err(self.refused(step));
        }

        serde_json::from_str(&self.body).map_err(|_| LoginError::Malformed { step })
    }

    /// This answer as a refusal: its status, and the issuer's own error code
    /// where there is one. Never the body.
    fn refused(&self, step: &'static str) -> LoginError {
        LoginError::Refused {
            step,
            status: self.status,
            code: self.code().unwrap_or_else(|| UNNAMED.to_owned()),
        }
    }

    /// The issuer's OAuth error code, if it gave one in the shape RFC 6749
    /// §5.2 defines.
    fn code(&self) -> Option<String> {
        let body: Value = serde_json::from_str(&self.body).ok()?;

        loopback::error_code(body.get("error")?.as_str()?)
    }
}

/// What a token endpoint answers with (`openai.ts:26-31`).
#[derive(Deserialize)]
struct Tokens {
    /// Who signed in. Carries the account id, when there is one.
    #[serde(default)]
    id_token: Option<SecretString>,
    /// What a request would carry.
    access_token: SecretString,
    /// What the next renewal spends. Absent on a renewal that did not rotate.
    #[serde(default)]
    refresh_token: Option<SecretString>,
    /// Seconds the access token lasts.
    #[serde(default)]
    expires_in: Option<u64>,
}

/// What the device endpoint answers a request for a code with
/// (`openai.ts:105`).
#[derive(Deserialize)]
struct DeviceCode {
    /// What identifies this login when polling.
    device_auth_id: String,
    /// What the person types.
    user_code: String,
    /// Seconds to leave between polls. A string in the shape upstream types;
    /// see [`poll_interval`].
    #[serde(default)]
    interval: Option<Value>,
}

/// What the device endpoint answers with once the code has been entered
/// (`openai.ts:131-134`).
#[derive(Deserialize)]
struct DeviceGrant {
    /// The code to exchange.
    authorization_code: SecretString,
    /// The verifier that proves it — minted by the server, not by this.
    code_verifier: SecretString,
}

/// The credential a *login's* token response becomes.
///
/// A login's answer has to carry a refresh token: there is no previous
/// credential to fall back on, and one without it expires in an hour with no
/// way back. Upstream types the field as required for the same reason
/// (`openai.ts:29`).
fn first_credential(tokens: Tokens, step: &'static str) -> Result<OauthCredential, LoginError> {
    let refresh = tokens
        .refresh_token
        .clone()
        .ok_or(LoginError::Malformed { step })?;

    Ok(credential(tokens, refresh))
}

/// The credential a token response becomes, given the refresh token to keep.
fn credential(tokens: Tokens, refresh: SecretString) -> OauthCredential {
    let account_id = account_id(&tokens);
    let mut credential =
        OauthCredential::new(refresh, tokens.access_token, expires(tokens.expires_in));
    credential.account_id = account_id;

    credential
}

/// When an access token stops being accepted.
///
/// Through [`now_ms`] and not a local clock read, because three login flows
/// computing this by hand is one of them computing it wrong.
fn expires(expires_in: Option<u64>) -> u64 {
    now_ms().saturating_add(
        expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN)
            .saturating_mul(1_000),
    )
}

/// The account a request is billed to (`openai.ts:275-292`, `codex.ts:38-76`).
///
/// The `id_token` first and the access token second, because only the first is
/// promised to carry an identity.
///
/// A token that will not decode, or that carries none of the three claims, is
/// **not a failed login**: the account id is optional on the credential and
/// most ChatGPT accounts have exactly one, so a missing claim is a login that
/// worked and a field left empty.
fn account_id(tokens: &Tokens) -> Option<String> {
    tokens
        .id_token
        .as_ref()
        .and_then(|token| claimed_account(token.expose_secret()))
        .or_else(|| claimed_account(tokens.access_token.expose_secret()))
}

/// The account id inside one token, in the order upstream reads them.
///
/// **The signature is never checked, and this is not validation.** Upstream
/// does not check it either (`codex.ts:47-55` is a base64 decode and a
/// `JSON.parse`), and checking it would need the issuer's keys, a fetch, a
/// cache and a rotation story — for a value that decides which of somebody's
/// own accounts a request is billed to. The issuer verifies its own token on
/// every call that carries it; this is a routing hint read out of a token this
/// process was just handed over TLS by the party that signed it. A test pins
/// that a token with a broken signature still yields its account id, so that
/// nobody later mistakes this for a check.
fn claimed_account(token: &str) -> Option<String> {
    let mut segments = token.split('.');
    segments.next()?;
    let payload = segments.next()?;
    segments.next()?;
    if segments.next().is_some() {
        return None;
    }

    let claims: Value = serde_json::from_slice(&CLAIMS.decode(payload).ok()?).ok()?;

    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|scoped| scoped.get("chatgpt_account_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|listed| listed.first())
                .and_then(|first| first.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// How long to leave between device polls.
///
/// Upstream is `Math.max(Number.parseInt(device.interval) || 5, 1) * 1000`
/// (`openai.ts:113`). The `|| 5` already absorbs zero and anything unparseable,
/// so the `Math.max(…, 1)` only ever guards a negative — which cannot survive
/// an unsigned parse here, and so has no counterpart.
///
/// The field is typed as a string upstream and arrives as one; a number is
/// accepted too, because a login that hard-failed on a JSON type changing would
/// be a login broken by a server becoming more conventional.
fn poll_interval(named: Option<&Value>) -> Duration {
    let seconds = named
        .and_then(|value| {
            value
                .as_str()
                .and_then(|text| text.parse::<u64>().ok())
                .or_else(|| value.as_u64())
        })
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_POLL_SECONDS);

    Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use secrecy::ExposeSecret as _;
    use serde_json::json;

    use super::{
        CALLBACK_PORT, DEFAULT_POLL_SECONDS, Device, Login, LoginError, PROVIDER_ID, Tokens,
        claimed_account, is_dead_grant, poll_interval,
    };
    use crate::auth::{AuthErrorKind, pkce};

    /// An account id no other value in a test could be mistaken for.
    const ACCOUNT: &str = "acct_2f7QpL9";

    /// A token that must never appear in anything a person or a log reads.
    const CANARY: &str = "sk-canary-DO-NOT-PRINT-8891";

    /// A JWT carrying `claims`, signed with nothing at all.
    ///
    /// The signature is deliberately not a signature: every test here is about
    /// a value read out of a token that was never verified, and a fixture that
    /// looked signed would suggest otherwise.
    fn token(claims: &serde_json::Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());

        format!("eyJhbGciOiJSUzI1NiJ9.{payload}.not-a-signature")
    }

    #[test]
    fn an_authorize_url_carries_every_parameter_the_issuer_requires() {
        let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
        let url = login.authorize_url("http://localhost:1455/auth/callback", "the-challenge", "st");
        let query: Vec<(String, String)> = url::Url::parse(&url)
            .expect("a URL")
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();

        assert!(
            url.starts_with("https://issuer.invalid/oauth/authorize?"),
            "{url}"
        );
        assert_eq!(
            query,
            vec![
                ("response_type".to_owned(), "code".to_owned()),
                (
                    "client_id".to_owned(),
                    "app_EMoamEEZ73f0CkXaXp7hrann".to_owned()
                ),
                (
                    "redirect_uri".to_owned(),
                    "http://localhost:1455/auth/callback".to_owned()
                ),
                (
                    "scope".to_owned(),
                    "openid profile email offline_access".to_owned()
                ),
                ("code_challenge".to_owned(), "the-challenge".to_owned()),
                ("code_challenge_method".to_owned(), "S256".to_owned()),
                ("id_token_add_organizations".to_owned(), "true".to_owned()),
                ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
                ("state".to_owned(), "st".to_owned()),
                ("originator".to_owned(), "opencode".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn the_challenge_a_login_publishes_is_the_digest_of_the_verifier_it_kept() {
        let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
        let browser = login.browser_on(0).await.expect("loopback is bindable");

        let published = url::Url::parse(browser.url())
            .expect("a URL")
            .query_pairs()
            .find(|(key, _)| key == "code_challenge")
            .map(|(_, value)| value.into_owned())
            .expect("the authorize URL publishes a challenge");

        assert_eq!(
            published,
            pkce::challenge_for(browser.pkce.verifier().expose_secret()),
            "the issuer recomputes this over the verifier the exchange presents"
        );
        assert_ne!(browser.port(), 0, "a bound socket has a real port");
    }

    #[tokio::test]
    async fn a_browser_login_names_the_port_it_is_actually_listening_on() {
        let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
        let browser = login.browser_on(0).await.expect("loopback is bindable");

        let redirect = url::Url::parse(browser.url())
            .expect("a URL")
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .map(|(_, value)| value.into_owned())
            .expect("the authorize URL names a redirect");

        assert_eq!(
            redirect,
            format!("http://localhost:{}/auth/callback", browser.port())
        );
        assert_ne!(
            browser.port(),
            CALLBACK_PORT,
            "port 0 is what keeps a test off the registered port"
        );
    }

    #[test]
    fn an_issuer_that_would_put_the_tokens_in_the_clear_is_refused() {
        for allowed in [
            "https://auth.openai.com",
            "http://127.0.0.1:8080",
            "http://localhost:9",
        ] {
            assert!(Login::with_issuer(allowed).is_ok(), "{allowed}");
        }
        for refused in [
            "http://auth.openai.com",
            "http://127.0.0.1.invalid",
            "http://localhost.invalid",
            "ftp://auth.openai.com",
            "not a url",
        ] {
            assert!(
                matches!(Login::with_issuer(refused), Err(LoginError::Issuer)),
                "{refused}"
            );
        }
    }

    #[test]
    fn an_issuer_carrying_a_secret_is_refused_before_it_can_reach_a_browser() {
        // The authorize URL is built by prefixing the issuer, and that URL is
        // printed to a terminal and opened in a browser; `reqwest` would also
        // turn userinfo into a `Basic` header on every token request. Both are
        // closed by refusing the issuer rather than by redacting it later.
        for carrying in [
            &format!("https://{CANARY}@issuer.invalid"),
            &format!("https://user:{CANARY}@issuer.invalid"),
            &format!("https://issuer.invalid?token={CANARY}"),
            &format!("https://issuer.invalid#{CANARY}"),
        ] {
            assert!(
                matches!(Login::with_issuer(carrying), Err(LoginError::Issuer)),
                "{carrying}"
            );
        }
    }

    #[test]
    fn a_trailing_slash_on_the_issuer_does_not_double_up_in_a_path() {
        let login = Login::with_issuer("https://issuer.invalid/").expect("https is allowed");

        assert!(
            login
                .authorize_url("http://localhost:1/cb", "c", "s")
                .starts_with("https://issuer.invalid/oauth/authorize?"),
            "{}",
            login.authorize_url("http://localhost:1/cb", "c", "s")
        );
    }

    #[test]
    fn an_account_id_is_read_from_each_claim_shape_in_priority_order() {
        // The flat claim wins over the namespaced one, which wins over the
        // first organization — upstream's own `??` chain (`openai.ts:284-288`).
        let all_three = json!({
            "chatgpt_account_id": ACCOUNT,
            "https://api.openai.com/auth": { "chatgpt_account_id": "namespaced" },
            "organizations": [{ "id": "organization" }],
        });
        assert_eq!(
            claimed_account(&token(&all_three)).as_deref(),
            Some(ACCOUNT)
        );

        let namespaced = json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": ACCOUNT },
            "organizations": [{ "id": "organization" }],
        });
        assert_eq!(
            claimed_account(&token(&namespaced)).as_deref(),
            Some(ACCOUNT)
        );

        let organizations = json!({ "organizations": [{ "id": ACCOUNT }, { "id": "second" }] });
        assert_eq!(
            claimed_account(&token(&organizations)).as_deref(),
            Some(ACCOUNT)
        );
    }

    #[test]
    fn a_token_nobody_could_verify_still_yields_its_account_id() {
        // The signature here is the string "not-a-signature". Nothing checks
        // it, and nothing should read this as though something did.
        let unverifiable = token(&json!({ "chatgpt_account_id": ACCOUNT }));

        assert!(unverifiable.ends_with(".not-a-signature"));
        assert_eq!(claimed_account(&unverifiable).as_deref(), Some(ACCOUNT));
    }

    #[test]
    fn a_padded_payload_decodes_like_an_unpadded_one() {
        // JWT producers differ on whether they pad. Refusing a padded payload
        // would be strictness with no security value on a value already not
        // trusted.
        let claims = json!({ "chatgpt_account_id": ACCOUNT });
        let payload = base64::engine::general_purpose::URL_SAFE.encode(claims.to_string());

        assert!(payload.ends_with('='), "the fixture has to be padded");
        assert_eq!(
            claimed_account(&format!("header.{payload}.signature")).as_deref(),
            Some(ACCOUNT)
        );
    }

    #[test]
    fn a_token_with_no_account_in_it_is_not_a_failure() {
        for shapeless in [
            token(&json!({ "email": "someone@example.invalid" })),
            token(&json!({ "organizations": [] })),
            token(&json!("not an object")),
            "two.segments".to_owned(),
            "four.segments.than.expected".to_owned(),
            "not-a-jwt".to_owned(),
            "header.!!!not-base64!!!.signature".to_owned(),
            String::new(),
        ] {
            assert_eq!(claimed_account(&shapeless), None, "{shapeless}");
        }
    }

    #[test]
    fn a_login_with_no_account_id_is_still_a_login() {
        let tokens: Tokens = serde_json::from_value(json!({
            "access_token": "at-1", "refresh_token": "rt-1", "id_token": "not-a-jwt",
        }))
        .expect("the shape decodes");
        let credential = super::first_credential(tokens, "testing").expect("a login");

        assert_eq!(credential.account_id, None);
        assert_eq!(credential.access.expose_secret(), "at-1");
    }

    #[test]
    fn a_login_whose_answer_carries_no_refresh_token_is_not_a_login() {
        // There would be no way back from the hour it lasts.
        let tokens: Tokens =
            serde_json::from_value(json!({ "access_token": "at-1" })).expect("the shape decodes");

        assert!(matches!(
            super::first_credential(tokens, "testing"),
            Err(LoginError::Malformed { .. })
        ));
    }

    #[test]
    fn only_a_4xx_that_is_not_a_rate_limit_means_logging_in_again() {
        for dead in [400, 401, 403, 404, 422, 499] {
            assert!(is_dead_grant(dead), "{dead}");
        }
        for survivable in [200, 429, 500, 502, 503] {
            assert!(!is_dead_grant(survivable), "{survivable}");
        }
    }

    #[test]
    fn a_refusal_and_a_transport_failure_are_never_the_same_answer() {
        let refused = LoginError::Refused {
            step: "renewing the credential",
            status: 401,
            code: "invalid_grant".to_owned(),
        }
        .into_auth(PROVIDER_ID);
        assert_eq!(refused.kind(), AuthErrorKind::ReauthRequired);
        assert!(refused.to_string().contains("invalid_grant"), "{refused}");

        let limited = LoginError::Refused {
            step: "renewing the credential",
            status: 429,
            code: "rate_limit_exceeded".to_owned(),
        }
        .into_auth(PROVIDER_ID);
        assert_eq!(limited.kind(), AuthErrorKind::RefreshUnavailable);

        let malformed = LoginError::Malformed {
            step: "renewing the credential",
        }
        .into_auth(PROVIDER_ID);
        assert_eq!(malformed.kind(), AuthErrorKind::RefreshUnavailable);

        let cancelled = LoginError::Cancelled.into_auth(PROVIDER_ID);
        assert_eq!(cancelled.kind(), AuthErrorKind::RefreshUnavailable);
    }

    #[test]
    fn no_failure_message_renders_a_token() {
        let messages = [
            LoginError::Refused {
                step: "renewing the credential",
                status: 401,
                // What a careless implementation would put here: the body.
                code: super::UNNAMED.to_owned(),
            }
            .into_auth(PROVIDER_ID)
            .to_string(),
            LoginError::Malformed {
                step: "exchanging the authorization code",
            }
            .into_auth(PROVIDER_ID)
            .to_string(),
            // A `Login` is held for the length of a login and lands in a
            // `tracing` field the moment anybody adds one, so its own rendering
            // has to be safe too. It is, because nothing it holds may carry a
            // secret — see the issuer check.
            format!(
                "{:?}",
                Login::with_issuer("https://issuer.invalid").expect("https is allowed")
            ),
        ];

        for message in messages {
            assert!(
                !message.contains(CANARY),
                "a secret reached a message: {message}"
            );
        }
    }

    #[test]
    fn the_poll_interval_is_the_one_the_issuer_named() {
        assert_eq!(
            poll_interval(Some(&json!("7"))),
            Duration::from_secs(7),
            "the field arrives as a string"
        );
        assert_eq!(
            poll_interval(Some(&json!(7))),
            Duration::from_secs(7),
            "and a number is not a reason to fail a login"
        );
        for absent in [
            None,
            Some(&json!("0")),
            Some(&json!("")),
            Some(&json!(null)),
        ] {
            assert_eq!(
                poll_interval(absent),
                Duration::from_secs(DEFAULT_POLL_SECONDS),
                "{absent:?}"
            );
        }
    }

    #[tokio::test]
    async fn nothing_renders_a_state_or_a_verifier_out_of_a_started_login() {
        // A login in flight is held for minutes, which is exactly the window in
        // which a `tracing` field or somebody's `{:?}` would put its secrets in
        // a log. Asserting that `SecretString` redacts would prove something
        // about the `secrecy` crate; this asserts it about the two types this
        // module hands a caller.
        let login = Login::with_issuer("https://issuer.invalid").expect("https is allowed");
        let browser = login.browser_on(0).await.expect("loopback is bindable");
        let rendered = format!("{browser:?}");

        let state = url::Url::parse(browser.url())
            .expect("a URL")
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("the authorize URL publishes a state");

        assert!(
            !rendered.contains(&state),
            "the value that decides whose callback is accepted reached a Debug: {rendered}"
        );
        assert!(
            !rendered.contains(browser.pkce.verifier().expose_secret()),
            "a verifier reached a Debug: {rendered}"
        );
        // The challenge is published in the same URL, so its presence is what
        // keeps the two assertions above from being vacuous.
        assert!(rendered.contains(browser.pkce.challenge()), "{rendered}");

        let device = Device {
            login,
            url: "https://issuer.invalid/codex/device".to_owned(),
            user_code: "ABCD-EFGH".to_owned(),
            device_auth_id: CANARY.to_owned(),
            interval: Duration::from_secs(5),
        };
        let rendered = format!("{device:?}");

        assert!(
            !rendered.contains(CANARY),
            "half of what claims the grant reached a Debug: {rendered}"
        );
        // The code is on a screen by design; showing it is what makes this
        // assertion about the *other* half rather than about redacting whatever
        // is nearest.
        assert!(rendered.contains("ABCD-EFGH"), "{rendered}");
    }

    #[test]
    fn the_registered_client_is_the_one_a_plain_login_talks_to() {
        // Every other test here points at an issuer of its own, so the two
        // constants a production login actually uses are otherwise unread — and
        // both belong to a client registration this project cannot change. A
        // typo in either fails nowhere but in front of a person.
        let login = Login::new().expect("the real issuer is https");

        assert!(
            login
                .authorize_url("http://localhost:1455/auth/callback", "c", "s")
                .starts_with("https://auth.openai.com/oauth/authorize?"),
            "openai.ts:15"
        );
        assert_eq!(CALLBACK_PORT, 1455, "openai.ts:16");
    }
}
