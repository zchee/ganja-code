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

    pre_split_chatgpt_login_only(home.path());

    // **AC-0.4.** A machine holding nothing but a ChatGPT login written before
    // **D555**. That entry sits under `openai`, which now names the platform
    // API and a key, so no wire can present it — and the tier declines to adopt
    // it rather than starting a session that would refuse at its first
    // request. What is left is the last tier, and its notice.
    let stale = provider::select(&Config::default()).expect("an unadoptable login is not fatal");
    assert_eq!(stale.provider.id(), fake::ID);
    assert!(
        stale.notice.is_some(),
        "a machine whose one login cannot be run is exactly the machine that \
         has to be told nothing real is answering"
    );

    // **AC-0.1.** Naming the platform explicitly with no key is refused at
    // startup, and the refusal names both doors: the variable this id reads,
    // and the id that spends what this machine is actually holding.
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", "openai");
    }
    let no_key = provider::select(&Config::default())
        .expect_err("the platform has no key here, and the stored login is not one");
    let rendered = no_key.to_string();
    for door in ["OPENAI_API_KEY", "ganja auth login chatgpt", "GANJA_PROVIDER=chatgpt"] {
        assert!(
            rendered.contains(door),
            "the refusal has to name every way out of it; {door} missing from {rendered}"
        );
    }

    // And the seat is selectable by name on the same machine — the pre-split
    // entry is not read for it either, so what it builds is a session whose
    // first request will name the login to run, never one quietly spending
    // somebody's platform key.
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", "chatgpt");
    }
    let seat = provider::select(&Config::default()).expect("the seat is built from its id alone");
    assert_eq!(seat.provider.id(), "chatgpt");
    assert_eq!(seat.model, "gpt-5.4", "the seat's default is its wire's, never the catalog's");
    // SAFETY: as above.
    unsafe {
        env::remove_var("GANJA_PROVIDER");
    }
}

/// Replaces the store with one holding a single pre-**D555** ChatGPT login: an
/// OAuth entry under `openai`, the id that used to carry both credentials.
///
/// Written as a file rather than through `auth::set_oauth` because that
/// function stores under the id it is given and the whole point of this fixture
/// is the id it is *not* given any more — there is no supported way left to
/// produce this entry, which is exactly why a test has to.
fn pre_split_chatgpt_login_only(data_home: &std::path::Path) {
    let path = auth::store_path().expect("the store has a path");
    assert!(path.starts_with(data_home), "the fixture must not reach a real store: {path:?}");

    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "openai": {
                "type": "oauth",
                "refresh": "rt-pre-split-fixture",
                "access": "at-pre-split-fixture",
                "expires": 4_102_444_800_000_u64,
            }
        }))
        .expect("the fixture serializes"),
    )
    .expect("the fixture writes");
    fs::write(auth::stamps_path().expect("the stamps have a path"), "{}")
        .expect("the stamps clear");

    // The store refuses a credential file other users can read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("the fixture is made private");
    }
}
