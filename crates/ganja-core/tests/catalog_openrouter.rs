//! The one thing a gateway provider's whole cataloged tier rests on: that the
//! id ganja selects it by is the id the published catalog files its rows under.
//!
//! `provider::openrouter`'s own tests can only prove the translation
//! ([`auth::provider_id_for_storage_key`]) is the identity, because the table a
//! unit-test process holds is the compiled-in snapshot and that tier carries no
//! rows for this vendor at all. This is the other half, against a real catalog
//! payload read through the real loader: rows arrive under `openrouter`, sized
//! and priced, and `models openrouter` and the `/model` chooser have something
//! to show.
//!
//! One test, in its own binary, because it points the catalog at a file and
//! turns fetching off for the whole process — and because the load it performs
//! replaces the table every other test in the binary would be reading.
//!
//! The payload is trimmed to four rows in the shape the endpoint publishes
//! rather than the vendor's whole roster: what is being proved is the seam, and
//! a fixture that had to be regenerated whenever a gateway added a model would
//! be a test about somebody else's product decisions.

use std::fs;

use ganja_core::catalog;
use ganja_core::provider::openrouter;

/// Two providers, because the interesting failure is rows landing under the
/// *wrong* id rather than not landing at all — `xai` is here to prove the
/// loader really does translate where a translation exists, so openrouter's
/// arriving unchanged means something.
const PAYLOAD: &str = r#"{
  "openrouter": {
    "id": "openrouter",
    "name": "OpenRouter",
    "env": ["OPENROUTER_API_KEY"],
    "api": "https://openrouter.ai/api/v1",
    "models": {
      "openai/gpt-5.4": {
        "id": "openai/gpt-5.4",
        "name": "GPT-5.4",
        "cost": { "input": 2.5, "output": 15.0, "cache_read": 0.25 },
        "limit": { "context": 1050000, "output": 128000 }
      },
      "anthropic/claude-sonnet-5": {
        "id": "anthropic/claude-sonnet-5",
        "name": "Claude Sonnet 5",
        "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.2, "cache_write": 2.5 },
        "limit": { "context": 1000000, "output": 128000 }
      },
      "openrouter/auto": {
        "id": "openrouter/auto",
        "name": "Auto Router",
        "limit": { "context": 2000000, "output": 2000000 }
      }
    }
  },
  "xai": {
    "id": "xai",
    "name": "xAI",
    "models": {
      "grok-4.5": {
        "id": "grok-4.5",
        "name": "Grok 4.5",
        "cost": { "input": 3.0, "output": 15.0 },
        "limit": { "context": 2000000, "output": 128000 }
      }
    }
  }
}"#;

#[test]
fn a_published_catalog_files_this_gateways_rows_under_the_id_ganja_selects_it_by() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let payload = home.path().join("api.json");
    fs::write(&payload, PAYLOAD).expect("the fixture is writable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", home.path());
        std::env::set_var(catalog::MODELS_PATH_ENV, &payload);
        std::env::set_var(catalog::DISABLE_FETCH_ENV, "1");
    }

    assert!(
        catalog::load_cached(),
        "the loader adopted the payload {} names",
        catalog::MODELS_PATH_ENV
    );

    assert!(
        catalog::carries(openrouter::ID),
        "the rows landed under some other id, which is the failure that costs \
         this provider its sizing, its pricing and its auto-compaction without \
         costing it a single error message"
    );

    let priced = catalog::model_for(openrouter::ID, "openai/gpt-5.4")
        .expect("the row the payload carries, under the provider it carries it for");
    assert_eq!(priced.provider_id, openrouter::ID);
    assert_eq!(priced.context_window, 1_050_000);
    assert_eq!(priced.max_output, 128_000);
    assert!(
        priced.pricing.input > 0.0 && priced.pricing.output > 0.0,
        "a gateway row this build cannot price is one it cannot report a cost \
         for either"
    );

    // The namespaced spelling is the whole id, not a provider prefix to be
    // stripped: `anthropic/claude-sonnet-5` is a row *of openrouter's*, and
    // reading it as anthropic's would price a gateway turn against the wrong
    // vendor's table.
    assert!(
        catalog::model_for(openrouter::ID, "anthropic/claude-sonnet-5").is_some(),
        "the namespace belongs to the model id"
    );
    assert!(
        catalog::model_for("anthropic", "anthropic/claude-sonnet-5").is_none(),
        "and not to the provider"
    );

    // The translation the loader really does perform, so that openrouter's
    // arriving unchanged is a fact about the id rather than about the loader
    // having no translation at all.
    assert!(
        catalog::model_for("grok", "grok-4.5").is_some(),
        "the payload spells this vendor upstream's way and ganja reads it as its \
         own"
    );

    // The pin decision, stated where a real catalog is loaded: rows for every
    // vendor this gateway fronts, and a default for none of them. See
    // `provider::openrouter` for the three reasons.
    assert_eq!(
        catalog::default_model(openrouter::ID),
        None,
        "a cataloged provider with no default is deliberate here, and a pin \
         added later should arrive with its own reason"
    );
}
