//! `auth.json` is shared territory, and a save must not cost somebody a login.
//!
//! An opencode install, a third-party plugin and this build can all be pointed
//! at the same file. Upstream's own read filters out what it cannot decode
//! (`packages/opencode/src/auth/index.ts:65-66`) and its write puts that
//! filtered map back (`:79`), so an entry it does not understand is gone the
//! next time it stores anything. Ganja goes through the file as JSON and only
//! ever replaces the one entry it was asked to replace — including the fields
//! *inside* an OAuth record, which upstream leaves open-ended by construction
//! (`provider/auth.ts:211-220` spreads `...extra` into what it persists).
//!
//! What this proves that the unit tests cannot: the whole public path, against
//! the real XDG resolution, with the file the store actually writes.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.

use std::{collections::BTreeSet, env, fs};

use ganja_core::auth::{self, CredentialKind, OauthCredential, Source};
use secrecy::{ExposeSecret as _, SecretString};

/// A credential file holding one of each: a key this build stores itself, an
/// OAuth record carrying two fields it has never heard of, and a credential
/// type nobody has invented yet.
fn fixture() -> serde_json::Value {
    serde_json::json!({
        "anthropic": { "type": "api", "key": "sk-anthropic-0001", "metadata": { "label": "work" } },
        "openai": {
            "type": "oauth",
            "refresh": "rt-openai-0002",
            "access": "at-openai-0003",
            "expires": 1_785_000_000_000_u64,
            "accountId": "acct-42",
            "chatgptPlanType": "pro",
            "someFuturePluginField": { "nested": [1, 2, 3] },
        },
        "some-future-provider": { "type": "quantum-handshake", "secret": "s", "rounds": 3 },
    })
}

fn stored() -> serde_json::Value {
    let path = auth::store_path().expect("the store has a path");

    serde_json::from_slice(&fs::read(path).expect("the store exists")).expect("the store is JSON")
}

#[test]
fn every_entry_survives_a_save_including_the_fields_this_build_cannot_read() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        // A stored credential is what has to answer here; an exported key would
        // win the lookup and the file would never be read.
        env::remove_var("ANTHROPIC_API_KEY");
        env::remove_var("OPENAI_API_KEY");
    }

    let path = auth::store_path().expect("the store has a path");
    fs::create_dir_all(path.parent().expect("the store is in a directory"))
        .expect("the directory is creatable");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&fixture()).expect("the fixture serializes"),
    )
    .expect("the fixture writes");
    #[cfg(unix)]
    std::fs::set_permissions(
        &path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .expect("the fixture is made private");

    // Every kind is recognised for what it is, and the one that is nobody's
    // business is left out of the listing rather than guessed at.
    let listed = auth::list_providers().expect("the listing reads");
    assert_eq!(
        listed
            .iter()
            .map(|entry| (entry.provider_id.as_str(), entry.kind, entry.source))
            .collect::<Vec<_>>(),
        vec![
            ("anthropic", CredentialKind::ApiKey, Source::File),
            ("openai", CredentialKind::Oauth, Source::File),
        ]
    );

    // The OAuth record comes back whole.
    let credential = auth::oauth_for("openai")
        .expect("the store reads")
        .expect("openai has an OAuth credential");
    assert_eq!(credential.refresh.expose_secret(), "rt-openai-0002");
    assert_eq!(credential.access.expose_secret(), "at-openai-0003");
    assert_eq!(credential.expires, 1_785_000_000_000);
    assert_eq!(credential.account_id.as_deref(), Some("acct-42"));
    assert_eq!(
        credential.extra.keys().collect::<BTreeSet<_>>(),
        [
            "chatgptPlanType".to_owned(),
            "someFuturePluginField".to_owned()
        ]
        .iter()
        .collect::<BTreeSet<_>>(),
        "the fields this build does not model are the ones it must not lose"
    );

    // A save of somebody else's provider leaves all three entries alone.
    auth::set_credential("github-copilot", "gho_unrelated_0004").expect("a new key stores");
    let after_unrelated_save = stored();
    for provider in ["anthropic", "openai", "some-future-provider"] {
        assert_eq!(
            after_unrelated_save[provider],
            fixture()[provider],
            "{provider} did not survive a save it had nothing to do with"
        );
    }

    // And storing the OAuth record that was just read puts back what was there
    // — every field, including the two nothing here understands.
    auth::set_oauth("openai", &credential).expect("the credential stores again");
    assert_eq!(
        stored()["openai"],
        fixture()["openai"],
        "storing what was read has to put back what was there"
    );

    // Replacing it replaces it: a login is not a merge, and a field belonging
    // to an account that has been logged out of must not outlive it.
    auth::set_oauth(
        "openai",
        &OauthCredential::new(
            SecretString::from("rt-openai-0005"),
            SecretString::from("at-openai-0006"),
            0,
        ),
    )
    .expect("a fresh credential stores");
    assert_eq!(
        stored()["openai"],
        serde_json::json!({
            "type": "oauth",
            "refresh": "rt-openai-0005",
            "access": "at-openai-0006",
            "expires": 0,
        })
    );
    for provider in ["anthropic", "some-future-provider"] {
        assert_eq!(
            stored()[provider],
            fixture()[provider],
            "{provider} did not survive a login it had nothing to do with"
        );
    }
}
