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
    116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212, 37, 77,
    105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
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
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

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
        assert!(seen.insert(bytes), "a 16-byte draw repeated within {DRAWS} draws");
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
