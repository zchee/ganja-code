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
mod tests {
    use std::collections::HashSet;

    use secrecy::ExposeSecret as _;

    use super::{Pkce, challenge_for, unguessable};

    /// RFC 7636 Appendix B's published verifier.
    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

    /// RFC 7636 Appendix B's published challenge for that verifier.
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    /// RFC 7636 Appendix B's published octets, which the verifier above is the
    /// base64url of.
    const RFC_OCTETS: [u8; 32] = [
        116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212, 37,
        77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
    ];

    /// Every character RFC 7636 §4.1 allows in a verifier.
    const UNRESERVED: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

    #[test]
    fn the_challenge_for_rfc_7636s_published_verifier_is_its_published_challenge() {
        assert_eq!(challenge_for(RFC_VERIFIER), RFC_CHALLENGE);
    }

    #[test]
    fn a_verifier_is_the_base64url_of_the_bytes_it_was_drawn_from() {
        // Appendix B's own worked example, which is what `unguessable` does:
        // the octets are the entropy, and the verifier is how they are spelled.
        // Checking this pins that the 43 characters are an *encoding* of 32
        // bytes rather than 43 characters sampled from an alphabet.
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        assert_eq!(URL_SAFE_NO_PAD.encode(RFC_OCTETS), RFC_VERIFIER);
    }

    #[test]
    fn a_generated_verifier_is_43_unreserved_characters() {
        for _ in 0..64 {
            let verifier = unguessable().expect("the platform has a random source");
            let verifier = verifier.expose_secret();

            assert_eq!(
                verifier.chars().count(),
                43,
                "RFC 7636 4.1 sets 43 characters as the floor: {verifier}"
            );
            for character in verifier.chars() {
                assert!(
                    UNRESERVED.contains(character),
                    "{character:?} is not in RFC 7636's unreserved set: {verifier}"
                );
            }
        }
    }

    #[test]
    fn two_logins_never_share_a_verifier_or_a_state() {
        // A `state` shared between two logins is a `state` a callback belonging
        // to the first can answer the second with, which is the whole attack
        // the value exists to refuse.
        const DRAWS: usize = 256;

        let mut seen = HashSet::new();
        for _ in 0..DRAWS {
            let value = unguessable().expect("the platform has a random source");
            assert!(
                seen.insert(value.expose_secret().to_owned()),
                "a value repeated within {DRAWS} draws"
            );
        }
    }

    #[test]
    fn raw_bytes_are_fresh_entropy_and_not_a_repeated_buffer() {
        // The failure this hunts is a buffer that is returned rather than
        // refilled: sixteen bytes colliding within 64 draws is not chance.
        const DRAWS: usize = 64;

        let mut seen = HashSet::new();
        for _ in 0..DRAWS {
            let bytes = super::random_bytes::<16>().expect("the platform has a random source");
            assert!(
                seen.insert(bytes),
                "a 16-byte draw repeated within {DRAWS} draws"
            );
        }
    }

    #[test]
    fn a_pairs_challenge_is_the_digest_of_the_verifier_it_kept() {
        let pkce = Pkce::generate().expect("the platform has a random source");

        assert_eq!(
            pkce.challenge(),
            challenge_for(pkce.verifier().expose_secret()),
            "the published digest has to be of the secret that will be presented"
        );
    }

    #[test]
    fn nothing_renders_a_verifier() {
        let pkce = Pkce::generate().expect("the platform has a random source");
        let rendered = format!("{pkce:?}");

        assert!(
            !rendered.contains(pkce.verifier().expose_secret()),
            "a verifier reached a Debug: {rendered}"
        );
        // The challenge is published in a URL, so it is not a secret and its
        // presence is what makes this assertion meaningful rather than vacuous.
        assert!(
            rendered.contains(pkce.challenge()),
            "the challenge is public and should still be legible: {rendered}"
        );
    }
}
