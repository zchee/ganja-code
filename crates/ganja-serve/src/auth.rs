//! HTTP Basic authentication, required exactly when a password is configured.
//!
//! Spec: upstream `packages/opencode/src/server/auth.ts` (the password and
//! username sources) and
//! `packages/opencode/src/server/routes/instance/httpapi/middleware/authorization.ts`
//! (the header, the `?auth_token=` escape hatch, and the challenge). The
//! environment variables are the `GANJA_`-spelled analogs of upstream's
//! `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME`, and the default
//! username is this build's own name the way upstream's is `"opencode"`.
//!
//! The `auth_token` query parameter is ported deliberately: an `EventSource`
//! cannot set a header, so an SSE client's only way to authenticate is the
//! URL. That is also why the serve layer never logs a query string.

use axum::http::HeaderMap;
use base64::Engine as _;
use secrecy::{ExposeSecret as _, SecretString};

/// Where the password comes from; upstream reads `OPENCODE_SERVER_PASSWORD`
/// (`server/auth.ts:18`).
pub const PASSWORD_ENV: &str = "GANJA_SERVER_PASSWORD";

/// Where the username comes from; upstream reads `OPENCODE_SERVER_USERNAME`
/// (`server/auth.ts:19`).
pub(crate) const USERNAME_ENV: &str = "GANJA_SERVER_USERNAME";

/// The username when [`USERNAME_ENV`] says nothing, as upstream defaults to
/// `"opencode"`.
pub(crate) const DEFAULT_USERNAME: &str = "ganja";

/// The query parameter that may carry the credential instead of the
/// `Authorization` header (`middleware/authorization.ts:12`).
pub(crate) const AUTH_TOKEN_QUERY: &str = "auth_token";

/// The challenge a `401` carries (`middleware/authorization.ts:14`).
pub(crate) const WWW_AUTHENTICATE: &str = "Basic realm=\"Secure Area\"";

/// The credential every request must present while one is configured.
///
/// The password lives in a [`SecretString`]: wiped on drop, and a `Debug`
/// rendering of this type — or of anything holding it — shows a redaction
/// rather than the secret.
#[derive(Clone, Debug)]
pub struct Credentials {
    /// Who logs in.
    pub username: String,
    /// What they know.
    pub password: SecretString,
}

impl Credentials {
    /// The credential the environment configures, or [`None`] when
    /// [`PASSWORD_ENV`] is unset or empty — upstream's `required` treats an
    /// empty password as no password at all (`server/auth.ts:24-26`).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let password = std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty())?;
        let username = std::env::var(USERNAME_ENV).unwrap_or_else(|_| DEFAULT_USERNAME.to_owned());

        Some(Self {
            username,
            password: SecretString::from(password),
        })
    }
}

/// Whether the request presented `expected`, reading the query's
/// [`AUTH_TOKEN_QUERY`] first and the `Authorization: Basic` header second,
/// upstream's order (`middleware/authorization.ts:77-83`).
pub(crate) fn authorized(headers: &HeaderMap, query: Option<&str>, expected: &Credentials) -> bool {
    let Some((username, password)) = presented(headers, query) else {
        return false;
    };

    // Both halves are folded whole rather than compared byte-for-byte with an
    // early exit, so a wrong guess costs the same time wherever it is wrong.
    let user_ok = eq_fold(username.as_bytes(), expected.username.as_bytes());
    let pass_ok = eq_fold(
        password.as_bytes(),
        expected.password.expose_secret().as_bytes(),
    );

    user_ok && pass_ok
}

/// The `user:password` pair the request carried, from wherever it carried it.
fn presented(headers: &HeaderMap, query: Option<&str>) -> Option<(String, String)> {
    let token = query
        .and_then(|query| query_param(query, AUTH_TOKEN_QUERY))
        .or_else(|| {
            let header = headers
                .get(axum::http::header::AUTHORIZATION)?
                .to_str()
                .ok()?;
            let (scheme, value) = header.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("basic")
                .then(|| value.trim().to_owned())
        })?;

    decode_credential(&token)
}

/// Splits base64 `user:password` the way upstream does
/// (`middleware/authorization.ts:57-71`): undecodable, or missing the colon,
/// is an empty credential — which then simply fails the comparison.
fn decode_credential(token: &str) -> Option<(String, String)> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;

    Some((username.to_owned(), password.to_owned()))
}

/// Equality as a fold over every byte: length first, then the OR of all the
/// XORs, so there is no position-dependent early exit to time.
fn eq_fold(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One percent-decoded value out of a raw query string, the subset of
/// `URLSearchParams` this crate needs: split on `&` and the first `=`, `+`
/// is a space, `%XX` is a byte.
pub(crate) fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(key)? == name {
            return percent_decode(value);
        }
    }

    None
}

/// Form-urlencoded decoding; [`None`] for a malformed escape, which callers
/// treat as a value that matches nothing.
fn percent_decode(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();

    while let [first, tail @ ..] = rest {
        match first {
            b'+' => {
                bytes.push(b' ');
                rest = tail;
            }
            b'%' => {
                let [high, low, tail @ ..] = tail else {
                    return None;
                };
                let byte = (hex(*high)? << 4) | hex(*low)?;
                bytes.push(byte);
                rest = tail;
            }
            byte => {
                bytes.push(*byte);
                rest = tail;
            }
        }
    }

    String::from_utf8(bytes).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
