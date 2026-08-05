//! Logging in to xAI without a browser on this machine, and staying logged in.
//!
//! Spec: upstream `packages/opencode/src/plugin/xai.ts`. Two of its three
//! login methods are ported elsewhere or not at all — the PKCE loopback method
//! (`:551-584`) needs a listener on a pinned port and belongs to the module
//! that owns loopback callbacks, and the "manually enter API Key" method
//! (`:619-622`) is what `ganja auth login` already does. What is here is the
//! device grant (`:585-618`), which is the one that works over SSH, in a
//! container, and on a machine whose browser is somewhere else entirely.
//!
//! **The provider is called `grok` here and `xai` on disk.** That is not an
//! inconsistency to tidy up: [`super::storage_key`] maps the one to the other
//! so that an `auth.json` shared with an opencode install keeps working, and
//! [`PROVIDER_ID`] is the single place ganja's own name for it is written.
//!
//! Nothing in this module writes a credential. A login hands back an
//! [`OauthCredential`] and the caller stores it, which is what makes a
//! cancelled login structurally unable to leave anything behind.

use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Map, Value};

use super::{
    AuthError, OauthCredential,
    device::{
        BodyEncoding, DeviceError, DeviceFlow, Tokens, UPSTREAM_USER_AGENT, form, json_object,
        positive_seconds_ms, reportable_code, text,
    },
    now_ms,
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

/// The credential a completed login stores.
///
/// The expiry is computed the way every login flow in this build computes one
/// — [`now_ms`] plus the lifetime in milliseconds (`xai.ts:610`) — through the
/// shared helper rather than a second hand-rolled clock.
///
/// A device grant that returned no refresh token leaves that field blank
/// rather than borrowing the access token for it. The credential still works
/// until it expires; what it cannot do is renew itself, and
/// [`Refresh::refresh`] says exactly that rather than presenting an access
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
        let client = reqwest::Client::builder()
            // Refused for the reason every credential-carrying request in this
            // build refuses them: the body holds a refresh token, and a client
            // that followed a 3xx would replay it at whatever the redirect
            // named.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| AuthError::RefreshUnavailable {
                provider_id: PROVIDER_ID.to_owned(),
                reason: error.without_url().to_string(),
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

/// Why a renewal was refused, in a form that can be shown and logged.
///
/// A status, plus the endpoint's own error code when what arrived looked like
/// one. Never the body: an OAuth token endpoint's error body routinely quotes
/// the request back, and the request held a refresh token.
fn refusal(status: u16, fields: &Map<String, Value>) -> String {
    fields.get("error").and_then(Value::as_str).map_or_else(
        || format!("HTTP {status}"),
        |code| format!("HTTP {status}, {}", reportable_code(code)),
    )
}

#[cfg(test)]
mod tests {
    use secrecy::{ExposeSecret as _, SecretString};

    use super::{
        super::{
            AuthError, AuthErrorKind, OauthCredential, REFRESH_SKEW_MS, RefreshOauth as _,
            device::harness::{Reply, TestClock, serve},
            now_ms, storage_key,
        },
        CLIENT_ID, PROVIDER_ID, Refresh, SCOPE, UPSTREAM_USER_AGENT, credential_from,
        device_flow_at,
    };

    /// A canary that must never reach a message, a rendering or a log.
    const REFRESH_CANARY: &str = "xai-refresh-canary-AAAA";

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
        assert!(request.has_header("user-agent", "opencode/1.18.13"));

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
}
