//! A session nothing named a provider for runs as the oldest stored login.
//!
//! The unit tests own the pieces — the stamp sidecar's ordering in
//! `ganja-provider`'s `auth`, the adoption rule in `provider` — and what this
//! pins is the chain end to end, against the real store resolution: the fake
//! fallback with its notice survives as the *final* tier only, a stored login
//! is adopted silently, the config's `default_provider` key outranks the
//! login ordering and is outranked by the environment, and an unknown id in
//! that key is refused at startup naming the key rather than a variable
//! nobody set.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads. `XDG_DATA_HOME` is redirected so the machine running the suite
//! cannot contribute — or receive — a login of its own.

use std::{env, fs};

use ganja_core::auth;
use ganja_core::config::Config;
use ganja_core::provider::{self, SelectionError, fake};

#[test]
fn a_session_nothing_named_defaults_to_the_oldest_stored_login() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::remove_var("GANJA_PROVIDER");
        env::remove_var("GANJA_MODEL");
        env::remove_var("ANTHROPIC_API_KEY");
        env::remove_var("OPENAI_API_KEY");
    }

    // No logins at all: the fake provider, and still with its notice — the
    // final fallback is the one degradation worth announcing.
    let empty = provider::select(&Config::default()).expect("the fake provider needs nothing");
    assert_eq!(empty.provider.id(), fake::ID);
    assert!(
        empty.notice.is_some(),
        "a machine with no logins is the one that has to be told nothing real answers"
    );

    // An exported key is a one-shot override, not a login: it must not steer
    // the default, or a borrowed shell borrows an identity.
    // SAFETY: as above.
    unsafe {
        env::set_var("ANTHROPIC_API_KEY", "sk-exported-0001");
    }
    let exported = provider::select(&Config::default()).expect("the fake provider needs nothing");
    assert_eq!(
        exported.provider.id(),
        fake::ID,
        "an environment key participated in the login ordering"
    );
    // SAFETY: as above.
    unsafe {
        env::remove_var("ANTHROPIC_API_KEY");
    }

    // Two logins land; the ordering is the sidecar's to decide, so the test
    // decides it by writing the sidecar rather than racing the clock.
    auth::set_credential("anthropic", "sk-stored-0001").expect("the login stores");
    auth::set_credential("openai", "sk-stored-0002").expect("the login stores");
    let stamps = auth::stamps_path().expect("the stamps have a path");
    fs::write(&stamps, r#"{"anthropic": 1000, "openai": 2000}"#).expect("the stamps rewrite");

    let oldest = provider::select(&Config::default()).expect("the stored key authenticates");
    assert_eq!(oldest.provider.id(), "anthropic");
    assert!(
        oldest.notice.is_none(),
        "a provider the user logged into is not a degradation: {:?}",
        oldest.notice
    );

    // Flip the ages and the default follows the stamps, not the names.
    fs::write(&stamps, r#"{"anthropic": 2000, "openai": 1000}"#).expect("the stamps rewrite");
    assert_eq!(
        provider::select(&Config::default()).expect("the stored key authenticates").provider.id(),
        "openai"
    );

    // The config's `default_provider` key outranks the login ordering…
    let config = Config { default_provider: Some("anthropic".to_owned()), ..Config::default() };
    let named = provider::select(&config).expect("the named provider has a stored key");
    assert_eq!(named.provider.id(), "anthropic");
    assert!(
        named.notice.is_none(),
        "a provider a config asked for was not defaulted: {:?}",
        named.notice
    );

    // …and the environment outranks the config key, exactly as it outranks
    // the config's `model` key.
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", "openai");
    }
    assert_eq!(
        provider::select(&config).expect("the variable names a stored login").provider.id(),
        "openai"
    );
    // SAFETY: as above.
    unsafe {
        env::remove_var("GANJA_PROVIDER");
    }

    // An id the key names that nothing ships or declares fails at startup,
    // naming the key — not a variable nobody set — and the id it carried.
    let wrong = Config { default_provider: Some("gemini".to_owned()), ..Config::default() };
    let refused = provider::select(&wrong).expect_err("no such provider");
    let SelectionError::Unknown { requested, named_by, .. } = &refused else {
        panic!("expected an unknown-provider refusal, got {refused:?}");
    };
    assert_eq!(requested, "gemini");
    assert!(
        named_by.contains("default_provider"),
        "the config key is what named the id: {named_by}"
    );
    let rendered = refused.to_string();
    assert!(rendered.contains("default_provider") && rendered.contains("gemini"), "{rendered}");
}
