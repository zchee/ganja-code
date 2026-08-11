//! The RFC 8628 device authorization grant, written once for every provider
//! that speaks it.
//!
//! Spec: upstream `packages/opencode/src/plugin/xai.ts:198-286` — the request,
//! the bounded poll, and the two defensive normalisations it carries — and
//! `packages/opencode/src/plugin/github-copilot/copilot.ts:222-336`, which is
//! the same flow with a JSON body and the same safety margin. Upstream writes
//! it twice, once per plugin, and the two copies disagree in ways that matter;
//! this is one engine that both providers parameterise, and every place the
//! two upstreams differ is resolved at the site with both citations.
//!
//! **Two calls, not one.** A login has to print the user code and the
//! verification URI *before* anything blocks, so [`DeviceFlow::start`] returns
//! the authorization and [`DeviceFlow::poll`] waits for it. Upstream has the
//! same seam — `authorize()` returns `{url, instructions, callback}` and the
//! caller shows the first two before awaiting the third (`copilot.ts:258-262`,
//! `xai.ts:599-603`).
//!
//! **Nothing here writes a credential.** `start` and `poll` return values; the
//! caller decides whether to store them. That is what makes "a login that was
//! cancelled stores nothing" a property of the shape rather than a promise
//! about the code — there is no store in this module's reach to write to.
//!
//! **The loop is bounded**, which upstream's Copilot loop is not
//! (`copilot.ts:263`, `while (true)` with no deadline). Upstream can afford it
//! because a person is watching a spinner they can interrupt; ganja's flow may
//! be driven by something that is not watching, so the deadline xAI computes
//! from `expires_in` (`xai.ts:243-250`) is applied to both providers.
//!
//! Every URL is injectable so that a test drives the flow against a loopback
//! socket. There is deliberately **no https-or-loopback guard** on them, unlike
//! `provider::check_base_url`: the production endpoints are the constants in
//! [`grok`](super::grok) and [`copilot`](super::copilot), and the only other
//! caller is a test pointing at `127.0.0.1`. A guard would add a failure mode
//! and close no gap.

use std::{fmt, sync::Arc, time::Duration};

use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::RedactedTail;

/// What both flows send as their `User-Agent`.
///
/// **Deliberately upstream's product name, not ganja's.** Both client ids
/// below are applications somebody else registered — GitHub's is opencode's
/// own (`copilot.ts:9`), xAI's is the one Grok-CLI ships and opencode reuses
/// (`xai.ts:7-10`) — and the header shape measured against the live endpoints
/// is this one. A different product name presented against another project's
/// registered client id is a combination nothing has tested, and a
/// device-authorization endpoint is the wrong place to find out; the spikes
/// that measured it are also not repeatable, because the credentials they ran
/// on are gone. The version is the pin this behaviour is a port of
/// (`.omc/reference/opencode-v1.18.13`) rather than ganja's own, because
/// `opencode/0.1.0` is a release that never existed and this string is meant
/// to name one that did.
///
/// **The cost, recorded honestly rather than argued away:** a `User-Agent` is
/// what a server attributes traffic to, so ganja's device-flow requests are
/// logged — and rate-limited — as opencode's. That is an externality on a
/// third party, and it is the reason this is a decision rather than an
/// obvious call. It is the user's to make, and it is made: the header shape is
/// specified. Changing it later is this one constant and three assertions.
///
/// **Recorded for the lane that lands Copilot's request path:** the literal
/// below is the only string the live spike ever exercised against
/// `api.githubcopilot.com`. The *chat* API headers were never proven with any
/// other value. That is a separate decision, on a separate host, and whoever
/// makes it should make it deliberately rather than inherit this one.
///
/// One constant, named for *which* of the two kinds of string it is, so that
/// the OpenAI login and the wire lane share this answer rather than each
/// inventing one.
pub const UPSTREAM_USER_AGENT: &str = "opencode/1.18.13";

/// Added to every wait, for clock skew and timer drift.
///
/// Upstream's, at the same three seconds in both plugins (`copilot.ts:12-14`,
/// "to avoid hitting the server slightly too early due to clock skew / timer
/// drift"; `xai.ts:31`).
pub const POLLING_SAFETY_MARGIN_MS: u64 = 3_000;

/// How long to wait between polls when the server named no usable interval
/// (`xai.ts:27`).
pub const DEFAULT_INTERVAL_MS: u64 = 5_000;

/// The shortest wait between polls, whatever the server asks for
/// (`xai.ts:28`). A server naming a sub-second interval is asking to be
/// hammered by every client that believed it.
pub const MIN_INTERVAL_MS: u64 = 1_000;

/// How much a `slow_down` adds to the interval, per RFC 8628 §3.5
/// (`xai.ts:29`).
const SLOW_DOWN_INCREMENT_MS: u64 = 5_000;

/// How long a device code is assumed to live when the server did not say
/// (`xai.ts:30`).
///
/// Load-bearing for Copilot rather than for xAI: Copilot's device-code
/// response is read for `{verification_uri, user_code, device_code, interval}`
/// and never for `expires_in` (`copilot.ts:251-256`), so this is the only
/// deadline its flow has.
pub const DEFAULT_EXPIRES_MS: u64 = 5 * 60 * 1000;

/// One deadline over connect, headers and body of a single request, so an
/// endpoint that accepts a connection and then says nothing cannot park the
/// login on it. The poll's own deadline bounds the loop; this bounds a step.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Stands in for an error code that did not look like one.
///
/// See [`reportable_code`].
const UNREPEATABLE: &str = "an error code this build will not repeat";

/// The passage of time, so that a test can drive the poll without spending it.
///
/// Upstream injects exactly these two for exactly this reason (`xai.ts:239`,
/// and the comment at `:218-222`: "Test-injectable so we can exercise
/// authorization_pending / slow_down branches without real waits"). Driving a
/// paused runtime clock instead would work for the sleeps and not for the
/// assertions: there is a live socket round trip inside every iteration, so
/// elapsed wall time is a racy thing to assert on and the sequence of waits
/// that were *asked for* is not.
#[async_trait::async_trait]
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;

    /// Waits `duration` out.
    async fn sleep(&self, duration: Duration);
}

/// Real time.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

#[async_trait::async_trait]
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        super::now_ms()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// How a flow's request parameters travel.
///
/// The two providers disagree, and it is not a detail: Copilot's endpoints
/// take a **JSON** body (`copilot.ts:236-245`), which is a divergence from
/// most device-flow implementations and from RFC 8628's own examples, while
/// xAI's take the form encoding the RFC describes (`xai.ts:87-93`,
/// `:202-205`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyEncoding {
    /// `application/x-www-form-urlencoded`, as RFC 8628 §3.1 specifies.
    Form,
    /// `application/json`, as GitHub's device endpoints accept.
    Json,
}

/// A device-code flow could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    /// The endpoint could not be reached at all.
    ///
    /// Distinct from every other variant on purpose: nothing was refused, so
    /// nothing needs to be re-authorized. Retrying is the answer.
    #[error("the {context} endpoint could not be reached ({reason})")]
    Unreachable {
        /// Which request it was, in words that name no URL.
        context: &'static str,
        /// What the client said, with any URL stripped off it.
        reason: String,
    },
    /// The endpoint answered with a status that ends the flow.
    #[error("the {context} endpoint answered {status}")]
    Status {
        /// Which request it was.
        context: &'static str,
        /// The status it answered with.
        status: u16,
    },
    /// The endpoint answered with something that is not the response it owes.
    ///
    /// The body is deliberately never quoted: a token-endpoint body holds
    /// tokens.
    #[error("the {context} endpoint answered with {detail}")]
    Malformed {
        /// Which request it was.
        context: &'static str,
        /// What was wrong with it, in words that quote nothing.
        detail: &'static str,
    },
    /// The person said no, or the provider said no on their behalf
    /// (`xai.ts:276-278`).
    #[error("the login was denied")]
    Denied,
    /// The code ran out before it was entered (`xai.ts:279-281`).
    #[error("the login code expired before it was entered; start the login again")]
    CodeExpired,
    /// The provider refused with an error this build does not act on
    /// (`xai.ts:282-283`, `copilot.ts:329`).
    #[error("the provider refused the login ({code})")]
    Refused {
        /// The provider's own error code, or a stand-in when what arrived did
        /// not look like one.
        code: String,
    },
    /// The code's lifetime elapsed while waiting for it to be entered
    /// (`xai.ts:285`).
    #[error("the login was not completed in time; start the login again")]
    DeadlineExceeded,
    /// The caller cancelled.
    #[error("the login was cancelled")]
    Cancelled,
}

/// An authorization in progress: what to show a person, and what to poll with.
///
/// [`device_code`](Self::device_code) is held as a secret because it is one:
/// anyone holding it can complete the login and collect the tokens. The user
/// code and the verification URI are the opposite — they exist to be shown.
#[derive(Clone)]
pub struct DeviceAuthorization {
    /// What the poll presents to claim the tokens.
    device_code: SecretString,
    /// The short code a person types into the verification page.
    pub user_code: String,
    /// The page a person opens to enter [`user_code`](Self::user_code).
    pub verification_uri: String,
    /// The same page with the code already in it, where the provider offers
    /// one (`xai.ts:598` prefers it for the browser it opens).
    pub verification_uri_complete: Option<String>,
    /// How long to wait between polls, already floored at
    /// [`MIN_INTERVAL_MS`].
    interval_ms: u64,
    /// When the code stops being claimable, in milliseconds since the epoch.
    deadline_ms: u64,
}

impl DeviceAuthorization {
    /// The page to send a browser to: the pre-filled one where there is one,
    /// the plain one otherwise (`xai.ts:598`).
    #[must_use]
    pub fn browser_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }

    /// How long the poll will wait between attempts, before the safety margin.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }

    /// When this authorization stops being claimable, in milliseconds since
    /// the Unix epoch.
    #[must_use]
    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
}

impl fmt::Debug for DeviceAuthorization {
    /// Hand-written because [`device_code`](Self::device_code) is a bearer
    /// credential in every sense that matters, and a derived `Debug` would put
    /// it in a log.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceAuthorization")
            .field("device_code", &RedactedTail::of_secret(&self.device_code))
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("interval_ms", &self.interval_ms)
            .field("deadline_ms", &self.deadline_ms)
            .finish()
    }
}

/// What a completed device flow hands back.
///
/// Both fields beyond the access token are optional because one of the two
/// providers omits each: Copilot's token response is `{access_token}` and
/// nothing else (`copilot.ts:280-284`), and xAI's `expires_in` is documented
/// upstream as not always present (`xai.ts:488-489`).
#[derive(Clone)]
pub struct Tokens {
    /// The token a request carries.
    pub access: SecretString,
    /// The token a new access token is obtained with, where the provider
    /// issues one.
    pub refresh: Option<SecretString>,
    /// How long the access token lives, in seconds.
    pub expires_in: Option<u64>,
}

impl fmt::Debug for Tokens {
    /// Hand-written for the reason [`OauthCredential`](super::OauthCredential)'s
    /// is: these are the secrets, and a derived `Debug` prints them.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("access", &RedactedTail::of_secret(&self.access))
            .field(
                "refresh",
                &self.refresh.as_ref().map(RedactedTail::of_secret),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// One provider's device-code endpoints, and the shape it wants them asked in.
pub struct DeviceFlow {
    client: reqwest::Client,
    clock: Arc<dyn Clock>,
    device_code_url: String,
    token_url: String,
    client_id: &'static str,
    scope: &'static str,
    encoding: BodyEncoding,
}

impl DeviceFlow {
    /// A flow against `device_code_url` and `token_url`.
    ///
    /// Redirects are refused, the way every credential-carrying request in
    /// this build refuses them (`provider/mod.rs:260-277`): a token exchange
    /// is a one-shot `POST` that never legitimately redirects, and a client
    /// that followed one would replay the body — which holds the device code,
    /// and later the refresh token — at whatever the redirect named.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceError::Unreachable`] when no HTTP client can be built,
    /// which is a TLS backend that will not initialise rather than anything
    /// about the endpoint.
    pub fn new(
        device_code_url: impl Into<String>,
        token_url: impl Into<String>,
        client_id: &'static str,
        scope: &'static str,
        encoding: BodyEncoding,
    ) -> Result<Self, DeviceError> {
        let client =
            super::login_client(REQUEST_TIMEOUT).map_err(|error| DeviceError::Unreachable {
                context: "device authorization",
                reason: error.without_url().to_string(),
            })?;

        Ok(Self {
            client,
            clock: Arc::new(SystemClock),
            device_code_url: device_code_url.into(),
            token_url: token_url.into(),
            client_id,
            scope,
            encoding,
        })
    }

    /// The same flow, measuring time with `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;

        self
    }

    /// Where an authorization is started.
    #[must_use]
    pub fn device_code_url(&self) -> &str {
        &self.device_code_url
    }

    /// Where a device code is exchanged for tokens.
    #[must_use]
    pub fn token_url(&self) -> &str {
        &self.token_url
    }

    /// Asks the provider to start an authorization, and returns what to show
    /// for it.
    ///
    /// Spec: `xai.ts:198-216` and `copilot.ts:234-256`. The response must
    /// carry all three of `device_code`, `user_code` and `verification_uri`;
    /// upstream checks the same three and refuses the flow without them
    /// (`xai.ts:212-214`).
    ///
    /// # Errors
    ///
    /// Returns [`DeviceError::Unreachable`] when the endpoint could not be
    /// reached, [`DeviceError::Status`] when it refused
    /// (`copilot.ts:247-249`), [`DeviceError::Malformed`] when what came back
    /// is not an authorization, and [`DeviceError::Cancelled`] when `cancel`
    /// fired.
    pub async fn start(
        &self,
        cancel: &CancellationToken,
    ) -> Result<DeviceAuthorization, DeviceError> {
        const CONTEXT: &str = "device authorization";

        if cancel.is_cancelled() {
            return Err(DeviceError::Cancelled);
        }

        let (status, body) = self
            .post(
                CONTEXT,
                &self.device_code_url,
                &[("client_id", self.client_id), ("scope", self.scope)],
                cancel,
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(DeviceError::Status {
                context: CONTEXT,
                status,
            });
        }

        let (Some(device_code), Some(user_code), Some(verification_uri)) = (
            text(&body, "device_code"),
            text(&body, "user_code"),
            text(&body, "verification_uri"),
        ) else {
            return Err(DeviceError::Malformed {
                context: CONTEXT,
                detail: "no device_code, user_code and verification_uri in it",
            });
        };

        Ok(DeviceAuthorization {
            device_code: SecretString::from(device_code),
            user_code,
            verification_uri,
            verification_uri_complete: text(&body, "verification_uri_complete"),
            // Floored, because a server naming a sub-second interval is asking
            // every client that believed it to hammer the endpoint
            // (`xai.ts:245-248`).
            interval_ms: positive_seconds_to_ms(body.get("interval"), DEFAULT_INTERVAL_MS)
                .max(MIN_INTERVAL_MS),
            deadline_ms: self.clock.now_ms().saturating_add(positive_seconds_to_ms(
                body.get("expires_in"),
                DEFAULT_EXPIRES_MS,
            )),
        })
    }

    /// Waits for `authorization` to be completed, and returns its tokens.
    ///
    /// The classification is RFC 8628 §3.5's, and the **order** of the checks
    /// is what lets one loop serve both providers, because the two upstreams
    /// disagree about where a pending answer lives:
    ///
    /// - GitHub answers `200` with `{"error":"authorization_pending"}` in the
    ///   body, so `copilot.ts:278` can treat any non-2xx as terminal.
    /// - xAI answers `400` with the error in the body, per the RFC, so
    ///   `xai.ts:260-262` returns only on a 2xx and reads the error off the
    ///   *failed* response.
    ///
    /// Reading the body first and consulting the status only when the body
    /// named no error satisfies both. Taking `copilot.ts:278` literally in a
    /// shared loop would end xAI's flow on its first poll.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceError::Denied`], [`DeviceError::CodeExpired`],
    /// [`DeviceError::Refused`] or [`DeviceError::Status`] when the provider
    /// ended it, [`DeviceError::DeadlineExceeded`] when the code's lifetime
    /// ran out, [`DeviceError::Unreachable`] when the endpoint went away, and
    /// [`DeviceError::Cancelled`] when `cancel` fired.
    pub async fn poll(
        &self,
        authorization: &DeviceAuthorization,
        cancel: &CancellationToken,
    ) -> Result<Tokens, DeviceError> {
        const CONTEXT: &str = "device token";

        let mut interval_ms = authorization.interval_ms;

        loop {
            if cancel.is_cancelled() {
                return Err(DeviceError::Cancelled);
            }
            if self.clock.now_ms() >= authorization.deadline_ms {
                return Err(DeviceError::DeadlineExceeded);
            }

            let (status, body) = self
                .post(
                    CONTEXT,
                    &self.token_url,
                    &[
                        ("client_id", self.client_id),
                        ("device_code", authorization.device_code.expose_secret()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ],
                    cancel,
                )
                .await?;

            if let Some(tokens) = tokens(&body) {
                return Ok(tokens);
            }

            match body.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    // RFC 8628 §3.5 says add five seconds; GitHub may name the
                    // new interval itself, and a numeric positive one wins
                    // (`copilot.ts:318-323`). The increment compounds and
                    // persists, which is xAI's reading (`xai.ts:272`,
                    // `intervalMs += …`) and the RFC's intent. It is a
                    // deliberate divergence from `copilot.ts:316`, which
                    // recomputes `(original + 5)` every time and so lets a
                    // second back-off request the same wait as the first.
                    interval_ms = positive_seconds_ms(body.get("interval"))
                        .unwrap_or_else(|| interval_ms.saturating_add(SLOW_DOWN_INCREMENT_MS))
                        .max(MIN_INTERVAL_MS);
                }
                Some("access_denied" | "authorization_denied") => return Err(DeviceError::Denied),
                Some("expired_token") => return Err(DeviceError::CodeExpired),
                Some(other) => {
                    return Err(DeviceError::Refused {
                        code: reportable_code(other),
                    });
                }
                // No token and no error. A non-2xx is the end of it
                // (`copilot.ts:278`, `xai.ts:283`); a 2xx that said nothing is
                // a provider still making up its mind, which upstream waits
                // out rather than failing on (`copilot.ts:331-332`).
                None if !(200..300).contains(&status) => {
                    return Err(DeviceError::Status {
                        context: CONTEXT,
                        status,
                    });
                }
                None => {}
            }

            // Clamped to what is left, so the last wait cannot overshoot the
            // deadline and turn a five-minute code into a ten-minute wait
            // (`xai.ts:263`, `:268`). `saturating_sub` because the request
            // itself takes time, and a request that outlived the deadline
            // leaves nothing to wait for.
            let remaining = authorization
                .deadline_ms
                .saturating_sub(self.clock.now_ms());
            let wait = interval_ms
                .saturating_add(POLLING_SAFETY_MARGIN_MS)
                .min(remaining);

            tokio::select! {
                () = cancel.cancelled() => return Err(DeviceError::Cancelled),
                () = self.clock.sleep(Duration::from_millis(wait)) => {}
            }
        }
    }

    /// Sends one request and reads its status and JSON body.
    ///
    /// A body that is not a JSON object reads as an empty one rather than as a
    /// failure, which is upstream's own tolerance (`xai.ts:262`,
    /// `.catch(() => ({}))`): the status still says what happened, and quoting
    /// what arrived is exactly what must not happen with a token endpoint's
    /// response.
    async fn post(
        &self,
        context: &'static str,
        url: &str,
        fields: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<(u16, Map<String, Value>), DeviceError> {
        let request = self
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, UPSTREAM_USER_AGENT);
        let request = match self.encoding {
            BodyEncoding::Form => request
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(form(fields)),
            BodyEncoding::Json => request.json(&object(fields)),
        };

        let exchange = async {
            let response = request.send().await?;
            let status = response.status().as_u16();

            Ok::<_, reqwest::Error>((status, response.text().await?))
        };

        let (status, body) = tokio::select! {
            () = cancel.cancelled() => return Err(DeviceError::Cancelled),
            result = exchange => result.map_err(|error| DeviceError::Unreachable {
                context,
                // Stripped of its URL: the message is shown and logged, and a
                // token endpoint's URL is not something to widen the exposure
                // of even though this build never puts a secret in a query.
                reason: error.without_url().to_string(),
            })?,
        };

        Ok((status, json_object(&body)))
    }
}

/// A response body read as a JSON object, or an empty one.
///
/// Anything else — HTML from a proxy, a bare string, nothing at all — reads as
/// empty rather than as a failure, which is upstream's own tolerance
/// (`xai.ts:262`, `.catch(() => ({}))`). The status still says what happened,
/// and the body of a token endpoint is the last thing that should be quoted
/// into an error on its way to a log.
pub(super) fn json_object(body: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(fields)) => fields,
        _ => Map::new(),
    }
}

/// The tokens a response carries, when it carries a usable access token.
///
/// A blank `access_token` is not one: it would be stored, sent, and refused at
/// the provider with a message about the request rather than about the login.
fn tokens(body: &Map<String, Value>) -> Option<Tokens> {
    let access = text(body, "access_token")?;

    Some(Tokens {
        access: SecretString::from(access),
        refresh: text(body, "refresh_token").map(SecretString::from),
        // Seconds, and the same normalisation the interval gets: a provider
        // that answered `"3600"` or `null` here should not produce a
        // credential that expires at the epoch.
        expires_in: positive_seconds_ms(body.get("expires_in")).map(|ms| ms / 1_000),
    })
}

/// A non-blank string field.
pub(super) fn text(body: &Map<String, Value>, field: &str) -> Option<String> {
    let value = body.get(field)?.as_str()?.trim();

    (!value.is_empty()).then(|| value.to_owned())
}

/// `value` read as a positive number of seconds, in milliseconds.
///
/// Spec: `xai.ts:224-235`, whose comment records the bug this prevents — a
/// `NaN` interval slips through `?? default` because `NaN` is `typeof number`,
/// reaches `setTimeout(_, NaN)`, is treated as `0`, and busy-loops until the
/// hard deadline. Rust cannot reproduce that failure by that route, but every
/// input that produces it upstream — a missing field, `null`, a string, zero,
/// a negative — reaches this function here too, and each of them has to land
/// on the default rather than on a wait of nothing.
///
/// Strings are parsed because upstream's `Number(value)` coerces them, so a
/// provider answering `"5"` is a provider both tools wait five seconds for.
/// An absurd value needs no clamp of its own: every wait is capped at what is
/// left before the deadline, so a million-second interval sleeps until the
/// code expires and the flow ends there.
pub(super) fn positive_seconds_ms(value: Option<&Value>) -> Option<u64> {
    let seconds = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(digits) => digits.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }

    // The cast saturates at `u64::MAX` rather than wrapping, so an absurd
    // value stays absurd instead of becoming a tight loop; the guard above
    // has already ruled out the negative and non-finite cases.
    Some((seconds * 1_000.0) as u64)
}

/// Same, falling back to `default_ms`.
fn positive_seconds_to_ms(value: Option<&Value>, default_ms: u64) -> u64 {
    positive_seconds_ms(value).unwrap_or(default_ms)
}

/// An error code as it may be repeated back to a person and into a log.
///
/// RFC 6749 §5.2's vocabulary is lowercase ASCII with underscores, and RFC
/// 8628 adds three more of the same shape. Anything else in that field — a
/// stack trace, a quoted request, a token an over-helpful gateway pasted in —
/// is replaced rather than carried into a message that will be shown and
/// logged. The length bound is what makes it a *code* rather than prose.
pub(super) fn reportable_code(code: &str) -> String {
    let shaped = !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));

    if shaped {
        code.to_owned()
    } else {
        UNREPEATABLE.to_owned()
    }
}

/// The fields as a JSON object, for the encoding that wants one.
fn object(fields: &[(&str, &str)]) -> Map<String, Value> {
    fields
        .iter()
        .map(|(field, value)| ((*field).to_owned(), Value::from(*value)))
        .collect()
}

/// The fields percent-encoded as `application/x-www-form-urlencoded`.
///
/// Through `url`'s serializer rather than `reqwest`'s `RequestBuilder::form`,
/// which lives behind a feature this workspace does not enable — and rather
/// than a hand-rolled encoder, which is how a `+` in a token becomes a space
/// at the far end. `url` is already a dependency of this crate, and
/// `form_urlencoded` is the crate that implements the encoding it is named
/// after.
pub(super) fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish()
}

#[cfg(test)]
pub(super) mod harness {
    //! A loopback endpoint and a clock, shared by the three modules that drive
    //! a device flow.
    //!
    //! Real bytes over a real socket, the way every other provider suite in
    //! this build works: what is asserted on is the request that was actually
    //! built. The clock is the opposite of real, and deliberately — see
    //! [`Clock`](super::Clock).

    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
    };

    /// One canned answer.
    pub(in crate::auth) struct Reply {
        status: u16,
        body: String,
    }

    impl Reply {
        /// An answer with `status` and `body`.
        pub(in crate::auth) fn new(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                body: body.into(),
            }
        }

        /// A `200` carrying `body`.
        pub(in crate::auth) fn ok(body: impl Into<String>) -> Self {
            Self::new(200, body)
        }
    }

    /// One request, as it arrived.
    #[derive(Clone, Debug)]
    pub(in crate::auth) struct Request {
        /// The request line and headers, verbatim.
        pub(in crate::auth) head: String,
        /// The body, verbatim.
        pub(in crate::auth) body: String,
    }

    impl Request {
        /// Whether the head carries `header: value`, case-insensitively on the
        /// name the way HTTP is.
        pub(in crate::auth) fn has_header(&self, header: &str, value: &str) -> bool {
            self.head.lines().any(|line| {
                line.split_once(": ").is_some_and(|(name, found)| {
                    name.eq_ignore_ascii_case(header) && found.trim() == value
                })
            })
        }

        /// The body read as form fields.
        ///
        /// Decoded by the same crate that encoded it, which is the point: a
        /// hand-rolled decoder here could agree with a hand-rolled encoder on
        /// a mistake and prove nothing.
        pub(in crate::auth) fn form(&self) -> HashMap<String, String> {
            url::form_urlencoded::parse(self.body.as_bytes())
                .map(|(field, value)| (field.into_owned(), value.into_owned()))
                .collect()
        }

        /// The body read as JSON.
        pub(in crate::auth) fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("the body is JSON")
        }

        /// The path the request line names.
        pub(in crate::auth) fn path(&self) -> &str {
            self.head
                .split_whitespace()
                .nth(1)
                .expect("a request line has a path")
        }
    }

    /// A loopback endpoint answering canned replies in order.
    pub(in crate::auth) struct Endpoint {
        /// What to point a flow at.
        pub(in crate::auth) url: String,
        requests: Arc<Mutex<Vec<Request>>>,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Endpoint {
        /// Every request that arrived, in order.
        pub(in crate::auth) fn requests(&self) -> Vec<Request> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// How many arrived.
        pub(in crate::auth) fn count(&self) -> usize {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }

        /// The `index`-th one.
        pub(in crate::auth) fn request(&self, index: usize) -> Request {
            self.requests()
                .get(index)
                .cloned()
                .unwrap_or_else(|| panic!("no request {index} arrived"))
        }
    }

    /// Serves `replies` in order, one per connection, then stops listening.
    ///
    /// Running out is deliberate rather than a wrap-around: a loop that polled
    /// more times than a test allowed for then meets a refused connection and
    /// says so, instead of quietly being served the first answer again.
    pub(in crate::auth) async fn serve(replies: Vec<Reply>) -> Endpoint {
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
                for reply in replies {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };

                    let request = read(&mut socket).await;
                    requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(request);

                    let response = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: \
                         {}\r\nconnection: close\r\n\r\n{}",
                        reply.status,
                        reply.body.len(),
                        reply.body
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

    /// Reads a whole request: head to the blank line, then `content-length`
    /// bytes of body.
    async fn read(socket: &mut TcpStream) -> Request {
        let mut buffer = Vec::new();
        let mut byte = [0_u8; 1];
        while !buffer.ends_with(b"\r\n\r\n") {
            match socket.read(&mut byte).await {
                Ok(0) | Err(_) => break,
                Ok(_) => buffer.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&buffer).into_owned();

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

        Request {
            head,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    /// A clock a test drives: nothing waits, and `now` advances by exactly
    /// what was asked for.
    ///
    /// That equivalence is what makes the deadline assertions exact — a poll
    /// that overshoots by a millisecond overshoots here too.
    #[derive(Default)]
    pub(in crate::auth) struct TestClock {
        state: Mutex<(u64, Vec<Duration>)>,
    }

    impl TestClock {
        /// A clock reading `now_ms`, having waited for nothing.
        pub(in crate::auth) fn at(now_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new((now_ms, Vec::new())),
            })
        }

        /// Every wait that was asked for, in order.
        pub(in crate::auth) fn waits(&self) -> Vec<Duration> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .1
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl super::Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0
        }

        async fn sleep(&self, duration: Duration) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.0 = state
                .0
                .saturating_add(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
            state.1.push(duration);
        }
    }

    /// A clock whose waits never end, so that only a cancellation can.
    #[derive(Default)]
    pub(in crate::auth) struct StalledClock;

    #[async_trait::async_trait]
    impl super::Clock for StalledClock {
        fn now_ms(&self) -> u64 {
            0
        }

        async fn sleep(&self, _duration: Duration) {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio_util::sync::CancellationToken;

    use super::{
        BodyEncoding, DEFAULT_EXPIRES_MS, DEFAULT_INTERVAL_MS, DeviceError, DeviceFlow,
        MIN_INTERVAL_MS, POLLING_SAFETY_MARGIN_MS,
        harness::{Endpoint, Reply, StalledClock, TestClock, serve},
        positive_seconds_ms, reportable_code,
    };

    /// A device-code answer with `interval` seconds between polls and a code
    /// good for `expires_in` seconds.
    fn authorization(interval: &str, expires_in: &str) -> String {
        format!(
            r#"{{"device_code":"dev-code","user_code":"WXYZ-1234",
                 "verification_uri":"https://example.invalid/device",
                 "interval":{interval},"expires_in":{expires_in}}}"#
        )
    }

    /// A flow pointed at `endpoint`, driven by `clock`.
    fn device_flow(
        endpoint: &Endpoint,
        clock: Arc<dyn super::Clock>,
        encoding: BodyEncoding,
    ) -> DeviceFlow {
        DeviceFlow::new(
            format!("{}/device", endpoint.url),
            format!("{}/token", endpoint.url),
            "test-client",
            "test-scope",
            encoding,
        )
        .expect("a client builds")
        .with_clock(clock)
    }

    #[tokio::test]
    async fn a_pending_authorization_is_polled_at_the_interval_the_server_named() {
        let clock = TestClock::at(1_000);
        let endpoint = serve(vec![
            Reply::ok(authorization("7", "600")),
            // GitHub's spelling: a 200 with the error in the body. A loop that
            // took `copilot.ts:278` literally would still pass this one.
            Reply::ok(r#"{"error":"authorization_pending"}"#),
            // xAI's spelling: the RFC's 400. A loop that took
            // `copilot.ts:278` literally fails here, which is the point.
            Reply::new(400, r#"{"error":"authorization_pending"}"#),
            Reply::ok(r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#),
        ])
        .await;
        let cancel = CancellationToken::new();
        let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");
        assert_eq!(started.interval(), Duration::from_secs(7));
        assert_eq!(started.deadline_ms(), 1_000 + 600_000);

        let tokens = flow.poll(&started, &cancel).await.expect("the login lands");

        assert_eq!(
            clock.waits(),
            vec![
                Duration::from_millis(7_000 + POLLING_SAFETY_MARGIN_MS),
                Duration::from_millis(7_000 + POLLING_SAFETY_MARGIN_MS),
            ],
            "each pending answer should have cost one wait of the server's \
             interval plus the safety margin"
        );
        assert_eq!(endpoint.count(), 4, "one start and three polls");
        assert_eq!(tokens.expires_in, Some(3_600));
    }

    #[tokio::test]
    async fn a_slow_down_lets_the_server_choose_the_wait_and_otherwise_compounds() {
        let clock = TestClock::at(0);
        let endpoint = serve(vec![
            Reply::ok(authorization("5", "600")),
            // No interval named: RFC 8628 §3.5's five seconds are added.
            Reply::new(400, r#"{"error":"slow_down"}"#),
            // Still nothing named: the increment compounds rather than being
            // recomputed from the original interval.
            Reply::new(400, r#"{"error":"slow_down"}"#),
            // Named, positive, numeric: the server's answer wins outright.
            Reply::new(400, r#"{"error":"slow_down","interval":11}"#),
            // The raised interval persists into the next ordinary wait.
            Reply::new(400, r#"{"error":"authorization_pending"}"#),
            Reply::ok(r#"{"access_token":"at-1"}"#),
        ])
        .await;
        let cancel = CancellationToken::new();
        let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");
        flow.poll(&started, &cancel).await.expect("the login lands");

        let margin = POLLING_SAFETY_MARGIN_MS;
        assert_eq!(
            clock.waits(),
            vec![
                Duration::from_millis(10_000 + margin),
                Duration::from_millis(15_000 + margin),
                Duration::from_millis(11_000 + margin),
                Duration::from_millis(11_000 + margin),
            ],
            "5s +5 = 10, +5 again = 15, then the server's 11 replaces it and stays"
        );
    }

    #[tokio::test]
    async fn every_terminal_error_ends_the_loop_where_it_stands() {
        for (body, status, expected) in [
            (r#"{"error":"access_denied"}"#, 400, "denied"),
            (r#"{"error":"authorization_denied"}"#, 400, "denied"),
            (r#"{"error":"expired_token"}"#, 400, "expired"),
            (r#"{"error":"invalid_client"}"#, 400, "refused"),
            // No error named at all, which only a status can end.
            (r#"{}"#, 503, "status"),
        ] {
            let clock = TestClock::at(0);
            let endpoint = serve(vec![
                Reply::ok(authorization("5", "600")),
                Reply::new(status, body),
                // Deliberately available: a loop that polled again would be
                // served this and succeed, so the count is what proves it did
                // not.
                Reply::ok(r#"{"access_token":"never"}"#),
            ])
            .await;
            let cancel = CancellationToken::new();
            let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

            let started = flow.start(&cancel).await.expect("the code is issued");
            let failure = flow
                .poll(&started, &cancel)
                .await
                .expect_err("a terminal error is not a login");

            assert!(
                matches!(
                    (&failure, expected),
                    (DeviceError::Denied, "denied")
                        | (DeviceError::CodeExpired, "expired")
                        | (DeviceError::Refused { .. }, "refused")
                        | (DeviceError::Status { .. }, "status")
                ),
                "{body} with {status} should be {expected}, got {failure:?}"
            );
            assert_eq!(
                endpoint.count(),
                2,
                "{body} should have ended the loop, not been polled past"
            );
            assert!(clock.waits().is_empty(), "{body} should have cost no wait");
        }
    }

    #[tokio::test]
    async fn a_garbage_interval_falls_back_to_the_default_instead_of_spinning() {
        for named in ["null", "0", "-5", r#""NaN""#, r#""""#, "false"] {
            let clock = TestClock::at(0);
            let endpoint = serve(vec![
                Reply::ok(authorization(named, "600")),
                Reply::new(400, r#"{"error":"authorization_pending"}"#),
                Reply::ok(r#"{"access_token":"at-1"}"#),
            ])
            .await;
            let cancel = CancellationToken::new();
            let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

            let started = flow.start(&cancel).await.expect("the code is issued");
            assert_eq!(
                started.interval(),
                Duration::from_millis(DEFAULT_INTERVAL_MS),
                "an interval of {named} is not an interval"
            );

            flow.poll(&started, &cancel).await.expect("the login lands");

            assert_eq!(
                clock.waits(),
                vec![Duration::from_millis(
                    DEFAULT_INTERVAL_MS + POLLING_SAFETY_MARGIN_MS
                )],
                "an interval of {named} should have waited the default, not nothing"
            );
        }
    }

    #[tokio::test]
    async fn an_interval_under_the_floor_is_raised_to_it() {
        let clock = TestClock::at(0);
        let endpoint = serve(vec![
            Reply::ok(authorization("0.2", "600")),
            Reply::new(400, r#"{"error":"authorization_pending"}"#),
            Reply::ok(r#"{"access_token":"at-1"}"#),
        ])
        .await;
        let cancel = CancellationToken::new();
        let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");
        flow.poll(&started, &cancel).await.expect("the login lands");

        assert_eq!(started.interval(), Duration::from_millis(MIN_INTERVAL_MS));
        assert_eq!(
            clock.waits(),
            vec![Duration::from_millis(
                MIN_INTERVAL_MS + POLLING_SAFETY_MARGIN_MS
            )]
        );
    }

    #[tokio::test]
    async fn a_code_that_is_never_entered_stops_at_its_deadline() {
        let clock = TestClock::at(0);
        // Four pending answers at 10s + 3s each is 52s of waiting against a
        // 30s code: the loop must stop before it runs out of answers.
        let endpoint = serve(
            std::iter::once(Reply::ok(authorization("10", "30")))
                .chain(
                    std::iter::repeat_with(|| {
                        Reply::new(400, r#"{"error":"authorization_pending"}"#)
                    })
                    .take(4),
                )
                .collect(),
        )
        .await;
        let cancel = CancellationToken::new();
        let flow = device_flow(&endpoint, clock.clone(), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");
        let failure = flow
            .poll(&started, &cancel)
            .await
            .expect_err("a code that expired is not a login");

        assert!(
            matches!(failure, DeviceError::DeadlineExceeded),
            "expected a deadline, got {failure:?}"
        );
        assert_eq!(
            clock.waits(),
            vec![
                Duration::from_millis(13_000),
                Duration::from_millis(13_000),
                // Clamped: 13s more would put the next poll 9s past a code
                // that has 4s left.
                Duration::from_millis(4_000),
            ],
            "the last wait must land on the deadline rather than past it"
        );
    }

    #[tokio::test]
    async fn a_code_with_no_stated_lifetime_still_gets_a_deadline() {
        let clock = TestClock::at(500);
        let endpoint = serve(vec![Reply::ok(
            r#"{"device_code":"dev","user_code":"WXYZ",
                "verification_uri":"https://example.invalid/device","interval":5}"#,
        )])
        .await;
        let flow = device_flow(&endpoint, clock, BodyEncoding::Json);

        let started = flow
            .start(&CancellationToken::new())
            .await
            .expect("the code is issued");

        assert_eq!(
            started.deadline_ms(),
            500 + DEFAULT_EXPIRES_MS,
            "GitHub never sends expires_in, so the default is the only bound its flow has"
        );
    }

    #[tokio::test]
    async fn a_cancelled_login_never_reaches_the_provider() {
        let clock = TestClock::at(0);
        let endpoint = serve(vec![Reply::ok(authorization("5", "600"))]).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let flow = device_flow(&endpoint, clock, BodyEncoding::Form);

        let failure = flow
            .start(&cancel)
            .await
            .expect_err("a cancelled login is not a login");

        assert!(matches!(failure, DeviceError::Cancelled), "got {failure:?}");
        assert_eq!(endpoint.count(), 0, "nothing should have been asked for");
    }

    #[tokio::test]
    async fn a_cancel_during_the_wait_ends_the_poll_promptly() {
        let endpoint = serve(vec![
            Reply::ok(authorization("5", "600")),
            Reply::new(400, r#"{"error":"authorization_pending"}"#),
            Reply::ok(r#"{"access_token":"never"}"#),
        ])
        .await;
        let cancel = CancellationToken::new();
        // A clock whose waits never end, so the only way out of the loop is
        // the cancellation itself.
        let flow = device_flow(&endpoint, Arc::new(StalledClock), BodyEncoding::Form);

        let started = flow.start(&cancel).await.expect("the code is issued");

        let waiting = tokio::spawn({
            let cancel = cancel.clone();
            async move { flow.poll(&started, &cancel).await }
        });
        while endpoint.count() < 2 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();

        let failure = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("a cancelled poll returns rather than hanging")
            .expect("the task did not panic")
            .expect_err("a cancelled login is not a login");

        assert!(matches!(failure, DeviceError::Cancelled), "got {failure:?}");
        assert_eq!(endpoint.count(), 2, "the poll should not have asked again");
    }

    #[tokio::test]
    async fn an_endpoint_that_is_not_there_is_reported_as_unreachable() {
        let endpoint = serve(Vec::new()).await;
        let flow = device_flow(&endpoint, TestClock::at(0), BodyEncoding::Form);

        let failure = flow
            .start(&CancellationToken::new())
            .await
            .expect_err("a closed listener answers nothing");

        assert!(
            matches!(failure, DeviceError::Unreachable { .. }),
            "got {failure:?}"
        );
    }

    #[tokio::test]
    async fn an_authorization_missing_what_it_is_for_is_refused() {
        let endpoint = serve(vec![Reply::ok(
            r#"{"user_code":"WXYZ","verification_uri":"https://example.invalid/d"}"#,
        )])
        .await;
        let flow = device_flow(&endpoint, TestClock::at(0), BodyEncoding::Form);

        let failure = flow
            .start(&CancellationToken::new())
            .await
            .expect_err("an authorization with no device code cannot be polled");

        assert!(
            matches!(failure, DeviceError::Malformed { .. }),
            "got {failure:?}"
        );
    }

    #[test]
    fn a_seconds_field_is_read_only_when_it_is_a_positive_number() {
        use serde_json::{Value, json};

        for named in [
            json!(null),
            json!(0),
            json!(-5),
            json!("NaN"),
            json!(""),
            json!(true),
        ] {
            assert_eq!(
                positive_seconds_ms(Some(&named)),
                None,
                "{named} is not a number of seconds"
            );
        }
        assert_eq!(positive_seconds_ms(None), None);
        assert_eq!(positive_seconds_ms(Some(&json!(7))), Some(7_000));
        // Coerced, because upstream's `Number("5")` is.
        assert_eq!(positive_seconds_ms(Some(&json!("5"))), Some(5_000));
        assert_eq!(positive_seconds_ms(Some(&json!(0.25))), Some(250));
        // Absurd rather than wrapped: the deadline is what bounds it.
        assert_eq!(
            positive_seconds_ms(Some(&Value::from(1e30))),
            Some(u64::MAX)
        );
    }

    #[test]
    fn an_error_code_that_is_not_one_is_not_repeated() {
        assert_eq!(reportable_code("invalid_grant"), "invalid_grant");
        assert_eq!(reportable_code("slow-down.2"), "slow-down.2");
        // The shape a token, a stack trace or a quoted request has.
        for unsafe_code in [
            "",
            "gho_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "invalid grant",
            "bearer=sk-1234",
        ] {
            assert_eq!(
                reportable_code(unsafe_code),
                super::UNREPEATABLE,
                "{unsafe_code:?} should not be repeated back"
            );
        }
    }
}
