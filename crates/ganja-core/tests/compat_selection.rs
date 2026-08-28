//! Naming a config-declared provider from the environment, and being refused
//! honestly when the name is one nothing has.
//!
//! The two wire suites beside this one reach a configured endpoint through the
//! config's own `model` key and through `--model`; this is the third route,
//! and the only one that cannot be driven without mutating the environment.
//! It is also where the refusal is observed end to end: the message's wording
//! is pinned as a unit test in `provider/mod.rs`, and what is proved here is
//! that `select` is what produces it, with this config's own entries in it.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.

use std::env;

use ganja_core::config::{Config, ProviderConfig};
use ganja_core::provider::{self, Dialect, PROVIDERS, SelectionError};

const CANARY: &str = "sk-test-canary-XYZ";
const KEY_VAR: &str = "GANJA_TEST_LOCAL_LLAMA_KEY";
const PROVIDER_ID: &str = "local-llama";
const MODEL: &str = "tiny-instruct";

/// A config declaring one endpoint, reachable but never actually dialled:
/// every assertion here is about *selection*, which happens before a request.
fn declaring() -> Config {
    let mut config = Config::default();
    config.provider.insert(
        PROVIDER_ID.to_owned(),
        ProviderConfig {
            dialect: Dialect::OpenaiChatCompletions,
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            key_env: Some(KEY_VAR.to_owned()),
            headers: std::collections::BTreeMap::new(),
        },
    );

    config
}

#[test]
fn the_provider_variable_names_a_configured_endpoint_and_refuses_the_rest_honestly() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var(KEY_VAR, CANARY);
        env::set_var("GANJA_PROVIDER", PROVIDER_ID);
        env::set_var("GANJA_MODEL", MODEL);
    }

    let config = declaring();
    let selection =
        provider::select(&config).expect("the variable names an endpoint this declares");
    assert_eq!(
        selection.provider.id(),
        PROVIDER_ID,
        "the environment reaches the config table exactly as the other tiers do"
    );
    assert_eq!(selection.model, MODEL);

    // The same variable, the same config, and a name nothing has. The refusal
    // has to name both tiers, because somebody who has just written an entry
    // and mistyped it needs to see what they actually wrote.
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", "local-lama");
    }
    let refused = provider::select(&config).expect_err("no such provider");
    let SelectionError::Unknown { requested, named_by, configured } = &refused else {
        panic!("expected an unknown-provider refusal, got {refused:?}");
    };
    assert_eq!(requested, "local-lama");
    assert_eq!(
        *named_by,
        provider::PROVIDER_ENV,
        "the variable is what named the id, and the refusal has to say so"
    );
    assert_eq!(configured, &[PROVIDER_ID.to_owned()]);

    let rendered = refused.to_string();
    assert!(rendered.contains("local-lama"), "{rendered}");
    assert!(
        rendered.contains(PROVIDER_ID),
        "the config's own endpoint is as selectable as a builtin, and a refusal \
         that hid it would tell somebody their entry does not exist: {rendered}"
    );
    for builtin in PROVIDERS {
        assert!(rendered.contains(builtin), "{builtin} is missing: {rendered}");
    }

    // A config declaring nothing gets the message it always had — an empty
    // list would read as "and this config names ", which is worse than silence.
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", "gemini");
    }
    let bare = provider::select(&Config::default()).expect_err("no such provider").to_string();
    assert!(
        !bare.contains("this config names"),
        "nothing was configured, so nothing should be listed: {bare}"
    );

    // And the credential is the entry's own: with the variable it names
    // cleared, the endpoint is refused at startup naming both places a key
    // could come from, rather than being built and failing at a request.
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", PROVIDER_ID);
        env::remove_var(KEY_VAR);
    }
    let unusable =
        provider::select(&config).expect_err("nothing supplies this endpoint's key").to_string();
    assert!(unusable.contains(KEY_VAR), "{unusable}");
    assert!(
        unusable.contains(&format!("ganja auth login {PROVIDER_ID}")),
        "the other place a key can come from is where the message has to point: {unusable}"
    );
    assert!(
        !unusable.contains(CANARY),
        "a refusal must not carry the credential it went looking for: {unusable}"
    );
}
