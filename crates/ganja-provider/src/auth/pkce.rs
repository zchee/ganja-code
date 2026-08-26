//! PKCE (RFC 7636) and the other unguessable values a login is built on.
//!
//! Spec: upstream `packages/core/src/plugin/provider/openai.ts:249-258`
//! (`generatePKCE`, `base64UrlEncode`) and `:49` (the `state`).
//!
//! Three values, one construction. The PKCE verifier proves to the token
//! endpoint that the code being exchanged was issued to whoever started the
//! login; the `state` proves to the *client* that the callback it just received
//! belongs to the login it started. Both are only worth anything if they cannot
//! be guessed, so both are thirty-two bytes of the operating system's entropy
//! rendered as base64url — the construction RFC 7636 Appendix B gives, whose
//! output is 43 characters drawn entirely from RFC 7636's unreserved set.
//!
//! **Deliberate divergence.** Upstream draws 43 bytes and maps each one modulo
//! a 66-character alphabet (`openai.ts:250-251`). That is biased — 256 is not a
//! multiple of 66, so the first 58 characters of the alphabet are ~1.5% more
//! likely than the last 8 — and it carries the entropy of 43 bytes through a
//! step that cannot preserve it. The provider never sees the verifier until the
//! exchange and only ever recomputes its SHA-256, so the two constructions are
//! interchangeable on the wire; the difference is only in how much a guess
//! costs. This one is also the one the RFC actually documents.
//!
//! The verifier is credential-grade for as long as a login is in flight: a
//! party holding it and an intercepted code can complete the exchange. It is
//! kept in a [`SecretString`] for that reason, and nothing here renders one.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret as _, SecretString};
use sha2::{Digest as _, Sha256};

/// Bytes drawn for each unguessable value.
///
/// Thirty-two, which is what RFC 7636 Appendix B encodes and what leaves the
/// 43-character verifier RFC 7636 §4.1 sets as its floor. Fewer would be
/// shorter than the RFC allows; more would only make the URL longer.
const BYTES: usize = 32;

/// The operating system would not supply the entropy a login is built on.
///
/// Not a condition any caller can retry its way out of, and not one that has
/// ever been observed on a platform this runs on — but the alternative to
/// reporting it is minting a predictable `state`, which is the one failure
/// this whole module exists to prevent.
#[derive(Debug, thiserror::Error)]
#[error("the operating system would not supply the 32 random bytes a login needs: {source}")]
pub struct EntropyError {
    /// What the platform's random source said.
    #[source]
    source: getrandom::Error,
}

/// A login's PKCE pair: the secret it keeps and the digest it publishes.
///
/// Held together because they are only ever correct together — a challenge
/// computed over anything but the verifier that will later be presented makes
/// the token endpoint refuse the exchange, and it refuses it at the very end of
/// a flow the person has already completed in a browser.
#[derive(Debug)]
pub struct Pkce {
    /// The secret, presented at the exchange.
    verifier: SecretString,
    /// Its S256 digest, published in the authorize URL.
    challenge: String,
}

impl Pkce {
    /// A fresh pair.
    ///
    /// # Errors
    ///
    /// Returns [`EntropyError`] when the platform's random source fails.
    pub fn generate() -> Result<Self, EntropyError> {
        let verifier = unguessable()?;
        let challenge = challenge_for(verifier.expose_secret());

        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// The verifier, for the one request that presents it.
    #[must_use]
    pub fn verifier(&self) -> &SecretString {
        &self.verifier
    }

    /// The challenge, for the authorize URL.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// The S256 challenge for `verifier`, as RFC 7636 §4.2 defines it.
///
/// The digest is taken over the **ASCII of the verifier string**, not over the
/// bytes it was rendered from. Both are 32-byte inputs to SHA-256 and both
/// produce a plausible-looking challenge, so the mistake survives every check
/// until the token endpoint recomputes it and refuses the exchange — at the
/// very end of a flow the person has already completed in a browser.
#[must_use]
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A fresh unguessable value: 32 bytes from the operating system, base64url.
///
/// The verifier, the `state` and the `nonce` are all this. Each login draws its
/// own of each, because a `state` reused across two logins is a `state` a
/// callback from the first can answer the second with.
///
/// # Errors
///
/// Returns [`EntropyError`] when the platform's random source fails.
pub fn unguessable() -> Result<SecretString, EntropyError> {
    Ok(SecretString::from(
        URL_SAFE_NO_PAD.encode(random_bytes::<BYTES>()?),
    ))
}

/// `N` bytes of the operating system's entropy, raw.
///
/// For the value whose *shape* is somebody else's to dictate: the cursor
/// login's pairing id is 16 bytes rendered as a UUID rather than 32 rendered
/// as base64url, and what has to be shared is the entropy source and its
/// failure report, not the spelling. [`unguessable`] is this plus the RFC 7636
/// rendering.
///
/// # Errors
///
/// Returns [`EntropyError`] when the platform's random source fails.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], EntropyError> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|source| EntropyError { source })?;

    Ok(bytes)
}

#[cfg(test)]
#[path = "pkce_tests.rs"]
mod tests;
