//! A grok session refuses at startup when there is no login to run as.
//!
//! Every other provider dies before the terminal is put into raw mode when its
//! credential is missing: the key providers name the environment variable they
//! wanted, and the ChatGPT path answers the same way when it finds neither a
//! key nor a stored login. Grok has no environment variable to name, and that
//! was the whole reason it used to start anyway — a session that looked fine
//! until the first thing somebody typed came back as a failed turn.
//!
//! What the probe is allowed to do is the other half of this drill. It asks the
//! store one question, *is there a credential*, and reads no token material to
//! answer it — so a canary planted in the store must not reach the refusal, the
//! provider's own rendering, or a `tracing` field. The capture is installed as
//! the global subscriber for the reason `secrets_env.rs` gives: a thread-local
//! one would quietly stop covering the library the day this got a multi-threaded
//! runtime, and the assertions would still pass against an empty search space.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`, and a plain
//! `cargo test` runs the tests inside a binary on parallel threads.

use std::{env, fs};

use ganja_core::auth::{self, OauthCredential, grok};
use ganja_core::provider::{GrokProvider, Provider as _, ProviderError};
use ganja_testkit::LogCapture as Capture;
use secrecy::SecretString;

/// The access token planted in the store. Nothing may render it.
const ACCESS_CANARY: &str = "at-grok-startup-canary-JKLM";

/// The refresh token planted beside it. Nothing may render this either, and it
/// is the one a probe that reached for "the credential" rather than "whether
/// there is one" would be holding.
const REFRESH_CANARY: &str = "rt-grok-startup-canary-NOPQ";

#[test]
fn a_grok_session_with_no_stored_login_refuses_before_it_starts() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
    }

    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .init();

    // Phase one: an empty store. The refusal names the command that fixes it,
    // the way the key providers name their variable.
    let refused = GrokProvider::from_stored()
        .expect_err("a session with no credential has nothing to run as");

    assert!(
        matches!(refused, ProviderError::Auth(_)),
        "nothing was refused by a network here: {refused:?}"
    );
    let message = format!("{refused} {refused:?}");
    assert!(
        message.contains("ganja auth login grok"),
        "the way out belongs in the message: {message}"
    );

    // Phase two: a login this provider can use. The probe is satisfied, and
    // what it read is not in anything that came out of it.
    auth::set_oauth(
        grok::PROVIDER_ID,
        &OauthCredential::new(
            SecretString::from(REFRESH_CANARY),
            SecretString::from(ACCESS_CANARY),
            auth::now_ms() + 86_400_000,
        ),
    )
    .expect("the credential stores");

    let provider = GrokProvider::from_stored().expect("a stored login is a session");
    assert_eq!(provider.id(), "grok");

    let rendered = format!("{provider:?} {message} {}", capture.logged());
    assert!(
        !rendered.contains(ACCESS_CANARY),
        "the probe asks whether there is a credential, never what it holds: {rendered}"
    );
    assert!(
        !rendered.contains(REFRESH_CANARY),
        "and the refresh token has one more way out than the access token does: \
         {rendered}"
    );
    // Without this the two assertions above could pass against nothing at all.
    assert!(
        fs::read_to_string(auth::store_path().expect("the store has a path"))
            .expect("the store was written")
            .contains(ACCESS_CANARY),
        "the canary has to be somewhere for its absence to mean anything"
    );

    // Phase three: a key rather than a login. `ganja auth login --provider
    // grok` stores one, and telling somebody who just ran that to run it again
    // would be the wrong sentence — a key stored for a provider that speaks
    // OAuth is `AuthError::NotOauth`'s situation, named at the first request
    // with what is actually stored.
    auth::remove_credential(grok::PROVIDER_ID).expect("the login is forgettable");
    auth::set_credential(grok::PROVIDER_ID, "xai-api-key-0001").expect("a key stores");

    assert!(
        GrokProvider::from_stored().is_ok(),
        "a stored key is a credential; which kind it is belongs to the request"
    );
}
