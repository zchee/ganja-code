//! A config `model` key belongs to the provider its prefix names.
//!
//! The rule itself is a unit test in `config` (`model_bound_to`); what this
//! pins is the tier that had the bug (`s4w`): with the environment naming one
//! provider and the config's `model` key naming another's model, selection
//! used to strip the prefix and forward the tail — a config
//! `model: "cursor/claude-x"` under `GANJA_PROVIDER=openai` reached the wire
//! as a bare `claude-x` and came back a live 400. It must instead pass the key
//! over and fall through to the next tier, in both the cataloged case (where
//! the next tier is the catalog's default) and the uncataloged one (where it
//! is the refusal naming the three ways to name a model).
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads. `XDG_DATA_HOME` is redirected so the machine running the suite
//! cannot contribute a login of its own, and the keys below are strings that
//! authenticate nothing — every assertion here is about *selection*, which
//! happens before a request.

use std::{collections::BTreeMap, env};

use ganja_core::{
    catalog,
    config::{Config, ProviderConfig},
    provider::{self, Dialect, SelectionError},
};

const COMPAT_ID: &str = "local-llama";
const COMPAT_KEY_VAR: &str = "GANJA_TEST_BINDING_LOCAL_LLAMA_KEY";

/// The config under test: one declared endpoint, and whatever `model` spelling
/// the case wants.
fn spelling(model: &str) -> Config {
    let mut config = Config {
        model: Some(model.to_owned()),
        ..Config::default()
    };
    config.provider.insert(
        COMPAT_ID.to_owned(),
        ProviderConfig {
            dialect: Dialect::OpenaiChatCompletions,
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            key_env: Some(COMPAT_KEY_VAR.to_owned()),
            headers: BTreeMap::new(),
        },
    );

    config
}

#[test]
fn a_config_model_reaches_only_the_provider_its_prefix_names() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var("GANJA_DISABLE_MODELS_FETCH", "1");
        env::set_var("ANTHROPIC_API_KEY", "sk-test-authenticates-nothing");
        env::set_var(COMPAT_KEY_VAR, "sk-test-authenticates-nothing");
        env::remove_var("GANJA_MODEL");
        env::set_var("GANJA_PROVIDER", "anthropic");
    }

    let cataloged_default =
        catalog::default_model("anthropic").expect("anthropic's default is pinned");

    // The bug, in the shape it was observed in: the environment names the
    // provider, the config names somebody else's model.
    let foreign = provider::select(&spelling("cursor/claude-x")).expect("the key is read as one");
    assert_eq!(foreign.provider.id(), "anthropic");
    assert_ne!(
        foreign.model, "claude-x",
        "the prefix named cursor; stripping it is what put a cursor model id \
         in an anthropic request"
    );
    assert_eq!(
        foreign.model, cataloged_default,
        "a passed-over key falls through to the next tier, which here is the \
         catalog's default for the provider that was actually selected"
    );

    // The same key, now naming the provider that is running: it applies, and
    // it is not the catalog's default, so this cannot pass by coincidence.
    let matching =
        provider::select(&spelling("anthropic/claude-x")).expect("the key is read as one");
    assert_eq!(matching.model, "claude-x");
    assert_ne!(matching.model, cataloged_default);

    // A bare spelling claims no provider, so it still applies to whoever is
    // running — the behavior nothing here was meant to change.
    let bare = provider::select(&spelling("claude-x")).expect("the key is read as one");
    assert_eq!(bare.model, "claude-x");

    // A config-declared endpoint is compared as the selected id whatever it
    // is, so its own spelling binds to it…
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", COMPAT_ID);
    }
    let declared = provider::select(&spelling("local-llama/tiny-instruct"))
        .expect("the declared endpoint has its key");
    assert_eq!(declared.provider.id(), COMPAT_ID);
    assert_eq!(declared.model, "tiny-instruct");

    // …and somebody else's spelling does not, which on an uncataloged
    // endpoint means the honest refusal rather than a wrong model: there is no
    // default to fall through to, and that is what the message says.
    let refused = provider::select(&spelling("anthropic/claude-x"))
        .expect_err("nothing can supply a model for an uncataloged endpoint");
    assert!(
        matches!(refused, SelectionError::NoDefaultModel { .. }),
        "expected the name-a-model refusal, got {refused:?}"
    );
    let rendered = refused.to_string();
    assert!(
        !rendered.contains("claude-x"),
        "the passed-over spec must not turn up as the model nothing could \
         supply: {rendered}"
    );
}
