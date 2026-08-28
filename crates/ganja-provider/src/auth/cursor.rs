//! Logging in to Cursor with a browser somewhere and a terminal that waits,
//! and staying logged in.
//!
//! Spec: the endpoint facts recorded in
//! `.omc/research/p7/scout-oauth-subscriptions.md` §3, extracted from
//! `ephraimduncan/opencode-cursor` `src/auth.ts` under a user-authorized read.
//! **Only the server-required interop facts are taken from there**: the three
//! URLs, the deep link's query parameters, "404 means pending", the poll body
//! carrying the tokens directly, and the refresh being a bearer-authenticated
//! `POST` with a `"{}"` body. Every client-side *choice* — the backoff
//! schedule, the wall-clock budget, the abort threshold — is ganja's own,
//! chosen and justified at the constant that holds it.
//!
//! The flow is neither RFC 8628 nor a loopback callback. The terminal mints a
//! PKCE pair and a pairing id, sends a browser to `cursor.com` carrying the
//! challenge and the id, and long-polls `api2.cursor.sh` with the id and the
//! verifier until the person finishes in the browser — the poll response
//! *is* the token delivery. There is no client id, no device code, no token
//! exchange and no socket bound here: the pairing id is the shared secret
//! between the browser tab and this terminal, which is why it is held in a
//! [`SecretString`] and never traced, even though it necessarily appears in
//! the URL a person is told to open.
//!
//! Nothing in this module writes a credential. A login hands back an
//! [`OauthCredential`] and the caller stores it, which is what makes a
//! cancelled login structurally unable to leave anything behind — the same
//! property every other flow under [`super`] is built on.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

use super::device::{Clock, SystemClock, json_object, text};
use super::pkce::{EntropyError, Pkce, random_bytes};
use super::{AuthError, OauthCredential, now_ms, token_deadline_ms};

/// What ganja calls this provider — and, uniquely among the OAuth providers,
/// also the key its credential is stored under: upstream's plugin files it
/// as `cursor`, so [`super::storage_key`] has no alias to apply.
pub const PROVIDER_ID: &str = "cursor";

/// Where the browser is sent (scout §3, `CURSOR_LOGIN_URL`).
const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";

/// Where the terminal polls for the tokens (scout §3, `CURSOR_POLL_URL`).
const POLL_URL: &str = "https://api2.cursor.sh/auth/poll";

/// Where a refresh token is traded for a fresh pair (scout §3,
/// `CURSOR_REFRESH_URL`).
const REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";

/// The first wait after a pending poll.
///
/// **Ganja's own constant**, like the three below it: the schedule is a
/// client-side choice, not a server requirement, so it is chosen here rather
/// than copied. One second keeps the happy path snappy — somebody already
/// signed in to cursor.com finishes the browser half in a couple of seconds,
/// and the terminal should move on the moment they have.
const INITIAL_POLL_MS: u64 = 1_000;

/// The wait the schedule levels off at.
///
/// Each pending poll doubles the wait until it lands here — 1s, 2s, 4s, 8s —
/// so a login that takes a while settles at one request every eight seconds.
/// Doubling reaches the cap in three steps, which spends only seven seconds
/// of extra latency on the way to a steady state gentle enough to hold for
/// the whole budget.
const MAX_POLL_MS: u64 = 8_000;

/// The whole login's wall-clock budget.
///
/// The same five minutes every other wait in this build gives a person —
/// [`super::grok::CALLBACK_DEADLINE`], [`super::device::DEFAULT_EXPIRES_MS`] —
/// because the question is the same one: how long an unattended terminal
/// should keep asking about a login nobody is finishing.
const POLL_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Consecutive poll failures that end the login.
///
/// One failure is routinely transient and two can be coincidence; three in a
/// row is a pattern, and each retry already costs a full backoff interval —
/// waiting out more of them would only stretch an outage into the deadline.
/// A pending answer resets the count: a server saying "not yet" is a server
/// answering fine.
const HARD_ERROR_LIMIT: u32 = 3;

/// How far ahead of the access token's own deadline the credential is marked
/// spent, per the recorded flow (scout §3): five minutes, so a turn started
/// near the boundary is not answered with a mid-flight 401.
const EXPIRY_MARGIN_MS: u64 = 5 * 60 * 1_000;

/// The lifetime assumed when the access token carries no readable `exp`
/// (scout §3's fallback): one hour from now, which errs toward refreshing
/// early rather than sending a token the server has already stopped taking.
const FALLBACK_LIFETIME_MS: u64 = 60 * 60 * 1_000;

/// One request's whole budget: connect, headers and body.
///
/// The poll is "long" in the sense of being repeated, not held open — a
/// pending answer is a prompt 404 — so the per-request bound every other
/// flow uses fits here too.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A cursor login could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The unguessable values a login is built on could not be drawn.
    #[error(transparent)]
    Entropy {
        /// What the platform's random source said.
        #[from]
        source: EntropyError,
    },
    /// No HTTP client could be built, which in practice means the TLS backend
    /// failed to initialize.
    #[error("no HTTP client for the cursor login: {source}")]
    Client {
        /// What the client builder said.
        #[source]
        source: reqwest::Error,
    },
    /// The poll endpoint failed `HARD_ERROR_LIMIT` times in a row.
    ///
    /// `reason` is a status or a transport report and never a response body:
    /// what a failing endpoint echoes back is nobody's to repeat, least of
    /// all on the path that carries a verifier in its query.
    #[error(
        "the cursor login was abandoned after {failures} consecutive poll failures; \
         the last was: {reason}"
    )]
    Aborted {
        /// How many failures ended it.
        failures: u32,
        /// What the last one was.
        reason: String,
    },
    /// Nobody completed the login in the browser in time.
    #[error("the cursor login was not completed within {}s", .after.as_secs())]
    TimedOut {
        /// How long was allowed.
        after: Duration,
    },
    /// The login was cancelled.
    #[error("the cursor login was cancelled")]
    Cancelled,
}

/// The login against Cursor's real endpoints.
///
/// # Errors
///
/// Returns [`LoginError::Client`] when no HTTP client can be built.
pub fn login_flow() -> Result<Flow, LoginError> {
    login_flow_at_urls(LOGIN_URL, POLL_URL)
}

/// The same login with **both** hosts redirected to `origin`, which is how a
/// test drives it against a loopback socket.
///
/// One origin overrides the pair deliberately: the deep link's page and the
/// poll endpoint live on different hosts in production, but a suite that owns
/// the poll has to be able to assert the deep link's shape too, so the
/// override puts `<origin>/loginDeepControl` and `<origin>/auth/poll` under
/// the same roof — the same paths the real hosts serve, so a redirected login
/// exercises the same routing.
///
/// # Errors
///
/// Returns [`LoginError::Client`] when no HTTP client can be built.
pub fn login_flow_at(origin: &str) -> Result<Flow, LoginError> {
    login_flow_at_urls(format!("{origin}/loginDeepControl"), format!("{origin}/auth/poll"))
}

/// The one constructor both routes above go through.
fn login_flow_at_urls(
    login_url: impl Into<String>,
    poll_url: impl Into<String>,
) -> Result<Flow, LoginError> {
    let client =
        super::login_client(REQUEST_TIMEOUT).map_err(|source| LoginError::Client { source })?;

    Ok(Flow {
        client,
        clock: Arc::new(SystemClock),
        login_url: login_url.into(),
        poll_url: poll_url.into(),
    })
}

/// Cursor's login: where to send somebody, and where to wait for what they
/// did there.
///
/// No `Debug`, deliberately — the same posture as [`super::grok::BrowserFlow`]:
/// the URLs are configuration a test may point anywhere, so nothing here can
/// promise a rendered one carries no secret, and a type that cannot be
/// formatted cannot leak one.
#[derive(Clone)]
pub struct Flow {
    /// What the polls go out on.
    client: reqwest::Client,
    /// The passage of time, injectable so a test can drive the backoff
    /// without spending it.
    clock: Arc<dyn Clock>,
    /// Where the browser is sent.
    login_url: String,
    /// Where the tokens are waited for.
    poll_url: String,
}

impl Flow {
    /// The same flow, measuring time with `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;

        self
    }

    /// Where the browser is sent.
    #[must_use]
    pub fn login_url(&self) -> &str {
        &self.login_url
    }

    /// Where the tokens are waited for.
    #[must_use]
    pub fn poll_url(&self) -> &str {
        &self.poll_url
    }

    /// Starts a login: mints the PKCE pair and the pairing id, and builds the
    /// URL to show.
    ///
    /// Nothing is sent and no socket is bound — the return path is the poll,
    /// not a callback — so this is synchronous where the other flows' starts
    /// are not.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::Entropy`] when the platform has no entropy.
    pub fn start(&self) -> Result<Login, LoginError> {
        let pkce = Pkce::generate()?;
        let uuid = pairing_id()?;
        // The parameters in the recorded order (scout §3):
        // `?challenge&uuid&mode=login&redirectTarget=cli`. `redirectTarget`
        // is what tells cursor.com the finish line is a polling terminal
        // rather than a redirect.
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("challenge", pkce.challenge())
            .append_pair("uuid", uuid.expose_secret())
            .append_pair("mode", "login")
            .append_pair("redirectTarget", "cli")
            .finish();
        let url = format!("{}?{query}", self.login_url);

        Ok(Login { flow: self.clone(), url, uuid, pkce })
    }
}

/// A cursor login whose URL is ready to open.
pub struct Login {
    /// The login this belongs to.
    flow: Flow,
    /// Where the person is sent.
    url: String,
    /// The value that pairs the browser tab with this terminal. A secret for
    /// as long as the login is in flight: whoever polls with it (and the
    /// verifier) collects the tokens.
    uuid: SecretString,
    /// The proof that the poll belongs to whoever opened the URL.
    pkce: Pkce,
}

impl fmt::Debug for Login {
    /// Hand-written because [`url`](Self::url) carries the pairing id in its
    /// query, exactly as [`super::grok::Browser`]'s carries its `state`. The
    /// challenge identifies a login for debugging and is published in that
    /// same URL, so it stays legible; the id and the verifier do not.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Login")
            .field("challenge", &self.pkce.challenge())
            .finish_non_exhaustive()
    }
}

/// What one poll came back as.
enum Poll {
    /// The tokens arrived.
    Ready(OauthCredential),
    /// A 404: nobody has finished in the browser yet.
    Pending,
    /// A transport failure, an unexpected status, or a success that carried
    /// no tokens — everything that counts toward [`HARD_ERROR_LIMIT`].
    Failed(String),
}

impl Login {
    /// Where the person has to go. Print it; open it if there is a browser.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Polls until the tokens arrive, the budget runs out, the endpoint fails
    /// `HARD_ERROR_LIMIT` times in a row, or `cancel` fires.
    ///
    /// The schedule is the one the constants above describe: 1s doubling to
    /// 8s, every wait clamped to what is left of the five-minute budget so
    /// the last one cannot overshoot it.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::TimedOut`], [`LoginError::Aborted`] or
    /// [`LoginError::Cancelled`], as above.
    pub async fn poll(self, cancel: &CancellationToken) -> Result<OauthCredential, LoginError> {
        // Built once: the pair is constant for the login's whole life, and
        // one spelling means the encoder cannot disagree with itself between
        // attempts.
        let url = {
            let query = form_urlencoded::Serializer::new(String::new())
                .append_pair("uuid", self.uuid.expose_secret())
                .append_pair("verifier", self.pkce.verifier().expose_secret())
                .finish();
            format!("{}?{query}", self.flow.poll_url)
        };
        let deadline = self
            .flow
            .clock
            .now_ms()
            .saturating_add(u64::try_from(POLL_DEADLINE.as_millis()).unwrap_or(u64::MAX));
        let mut interval_ms = INITIAL_POLL_MS;
        let mut failures: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                return Err(LoginError::Cancelled);
            }
            if self.flow.clock.now_ms() >= deadline {
                return Err(LoginError::TimedOut { after: POLL_DEADLINE });
            }

            match self.poll_once(&url, cancel).await? {
                Poll::Ready(credential) => return Ok(credential),
                Poll::Pending => failures = 0,
                Poll::Failed(reason) => {
                    failures += 1;
                    if failures >= HARD_ERROR_LIMIT {
                        return Err(LoginError::Aborted { failures, reason });
                    }
                }
            }

            // Clamped to what is left, so the last wait cannot overshoot the
            // deadline; `saturating_sub` because the request itself took time.
            let remaining = deadline.saturating_sub(self.flow.clock.now_ms());
            let wait = interval_ms.min(remaining);
            tokio::select! {
                () = cancel.cancelled() => return Err(LoginError::Cancelled),
                () = self.flow.clock.sleep(Duration::from_millis(wait)) => {}
            }
            interval_ms = interval_ms.saturating_mul(2).min(MAX_POLL_MS);
        }
    }

    /// One poll: pending, ready, or a failure the loop counts.
    ///
    /// Only a cancellation is an `Err` — everything the endpoint can do wrong
    /// is a [`Poll::Failed`] for the loop's counter, because a single bad
    /// answer must not end a login somebody is halfway through in a browser.
    async fn poll_once(&self, url: &str, cancel: &CancellationToken) -> Result<Poll, LoginError> {
        let exchange = async {
            let response = self
                .flow
                .client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await?;
            let status = response.status().as_u16();

            Ok::<_, reqwest::Error>((status, response.text().await.unwrap_or_default()))
        };

        let sent = tokio::select! {
            () = cancel.cancelled() => return Err(LoginError::Cancelled),
            result = exchange => result,
        };
        let (status, body) = match sent {
            Ok(answered) => answered,
            // Stripped of its URL: the poll's query carries the verifier and
            // the pairing id, and `reqwest` renders request URLs into its
            // messages.
            Err(error) => return Ok(Poll::Failed(error.without_url().to_string())),
        };

        // The recorded pending signal (scout §3): a 404, not an error body.
        if status == 404 {
            return Ok(Poll::Pending);
        }

        let fields = json_object(&body);
        if (200..300).contains(&status) {
            // The poll body is the token delivery, under cursor's own key
            // spellings (scout §3): `accessToken`, `refreshToken`.
            if let Some(access) = text(&fields, "accessToken") {
                return Ok(Poll::Ready(credential_from(
                    SecretString::from(access),
                    text(&fields, "refreshToken").map(SecretString::from),
                )));
            }

            // A success carrying no tokens is the server answering
            // unintelligibly; treating it as pending would poll a broken
            // server until the deadline.
            return Ok(Poll::Failed(format!("HTTP {status} carrying no tokens")));
        }

        // Never the body: whatever a failing endpoint echoes back is not
        // this build's to repeat.
        Ok(Poll::Failed(format!("HTTP {status}")))
    }
}

/// The credential a completed login — or a completed renewal — stores.
///
/// A delivery that returned no refresh token leaves that field blank rather
/// than borrowing the access token for it: the credential works until it
/// expires, and [`Refresh`] says exactly what it cannot do then.
fn credential_from(access: SecretString, refresh: Option<SecretString>) -> OauthCredential {
    let expires = expiry(&access);

    OauthCredential::new(refresh.unwrap_or_else(|| SecretString::from("")), access, expires)
}

/// When the credential should be renewed: the access token's own `exp` less
/// [`EXPIRY_MARGIN_MS`], or [`FALLBACK_LIFETIME_MS`] from now for a token
/// whose claims will not read.
///
/// The token's word is taken here because cursor's delivery carries no
/// `expires_in` beside it — unlike every other flow in this build, the JWT is
/// the *only* source of a deadline, so the margin is applied at storage time
/// rather than at [`OauthCredential::needs_refresh_for`]'s read.
fn expiry(access: &SecretString) -> u64 {
    match token_deadline_ms(access) {
        Some(deadline) => deadline.saturating_sub(EXPIRY_MARGIN_MS),
        None => now_ms().saturating_add(FALLBACK_LIFETIME_MS),
    }
}

/// A fresh pairing id: 16 bytes of the operating system's entropy in the
/// shape `crypto.randomUUID()` mints.
///
/// The RFC 9562 §5.4 version and variant bits are set because the id is a
/// value cursor's server pairs a browser tab against, and an id the server
/// might validate is minted in the shape the recorded client sends. What
/// makes it unguessable is the 122 random bits, not the format — which is
/// why the bytes are still drawn here while the format is [`uuid`]'s.
fn pairing_id() -> Result<SecretString, EntropyError> {
    let id = uuid::Builder::from_random_bytes(random_bytes::<16>()?).into_uuid();

    Ok(SecretString::from(id.hyphenated().to_string()))
}

/// Renews a cursor credential from the refresh token stored beside it.
///
/// The recorded request (scout §3): a `POST` whose `Authorization` bears the
/// *refresh* token and whose body is the literal `"{}"`, answered by the same
/// `{accessToken, refreshToken}` delivery the poll ends in. The refresh token
/// **rotates**: the endpoint hands back a new one and considers the old one
/// spent, so the new one is what gets stored — falling back to the old only
/// when the endpoint returned none, because guessing wrong in that direction
/// costs a login.
///
/// This type does no storing of its own. [`super::Refresher`] is what holds
/// the renewal to one at a time and writes the result down; this is only the
/// endpoint half, which is the split [`super::RefreshOauth`] exists to make.
pub struct Refresh {
    client: reqwest::Client,
    refresh_url: String,
}

impl Refresh {
    /// A refresher against cursor's real endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::RefreshUnavailable`] when no HTTP client can be
    /// built — a TLS backend that will not initialise, which is transient in
    /// the sense that matters: nothing was refused.
    pub fn new() -> Result<Self, AuthError> {
        Self::at(REFRESH_URL)
    }

    /// The same, against an endpoint of the caller's choosing.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::RefreshUnavailable`] when no HTTP client can be
    /// built.
    pub fn at(refresh_url: impl Into<String>) -> Result<Self, AuthError> {
        let client = super::login_client(REQUEST_TIMEOUT).map_err(|error| {
            AuthError::RefreshUnavailable {
                provider_id: PROVIDER_ID.to_owned(),
                reason: error.without_url().to_string(),
            }
        })?;

        Ok(Self { client, refresh_url: refresh_url.into() })
    }
}

#[async_trait::async_trait]
impl super::RefreshOauth for Refresh {
    /// Trades `credential`'s refresh token for a fresh pair.
    ///
    /// The classification is [`super::grok::Refresh`]'s, for its reasons: a
    /// 4xx means the credential is dead and only a new login fixes it
    /// ([`AuthError::ReauthRequired`]); a transport failure or a 5xx means
    /// the stored credential is untouched and retrying is the answer
    /// ([`AuthError::RefreshUnavailable`]). Neither `reason` ever carries a
    /// response body or a token.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::ReauthRequired`] or
    /// [`AuthError::RefreshUnavailable`], as above.
    async fn refresh(
        &self,
        provider_id: &str,
        credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        let stored = credential.refresh.expose_secret();
        if stored.trim().is_empty() {
            // Nothing to present. Saying so beats a round trip that can only
            // come back as the same answer.
            return Err(AuthError::ReauthRequired {
                provider_id: provider_id.to_owned(),
                reason: "no refresh token is stored beside it".to_owned(),
            });
        }

        let sent = self
            .client
            .post(&self.refresh_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {stored}"))
            // The recorded body is the literal empty object, not an empty
            // body: the endpoint is a JSON API and says so.
            .body("{}")
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
        let fields = json_object(&response.text().await.unwrap_or_default());

        if !status.is_success() {
            // Carries the provider and the status and nothing else: the
            // response to a refused renewal is nobody's to quote, and the
            // request it may echo held a refresh token.
            tracing::debug!(
                provider = %provider_id,
                status = status.as_u16(),
                "the cursor endpoint would not renew the stored credential",
            );

            let reason = format!("HTTP {}", status.as_u16());
            return Err(if status.is_client_error() {
                AuthError::ReauthRequired { provider_id: provider_id.to_owned(), reason }
            } else {
                AuthError::RefreshUnavailable { provider_id: provider_id.to_owned(), reason }
            });
        }

        let Some(access) = text(&fields, "accessToken") else {
            return Err(AuthError::ReauthRequired {
                provider_id: provider_id.to_owned(),
                reason: "the endpoint returned no access token".to_owned(),
            });
        };

        Ok(credential_from(
            SecretString::from(access),
            // Rotated, with the old one kept when the endpoint sent no new
            // one: it is only presumed live because the endpoint said
            // nothing, and presuming the other way discards the one token
            // that could still renew the credential.
            Some(
                text(&fields, "refreshToken")
                    .map_or_else(|| credential.refresh.clone(), SecretString::from),
            ),
        ))
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
