//! The wire model listing, asked about openai as each kind of credential.
//!
//! The seam's openai arm is a fact about the *credential*, not about the
//! provider name (**D476**): a stored ChatGPT login is a subscription seat and
//! is offered the pinned five, while an API key session and a machine holding
//! nothing at all both answer [`None`] so the catalog keeps describing openai.
//! All three of those are readings of the environment and of the credential
//! store, so all three are pinned here rather than in the crate's own tests,
//! where the verdict would be whatever the developer happens to be logged into.
//!
//! One test, one binary, on purpose: the three credential situations are three
//! settings of the same process-wide variables, so they are walked in sequence
//! by a single test — a plain `cargo test` runs a binary's tests on parallel
//! threads, and two of these racing would each see the other's environment.
//!
//! Nothing here reaches the network, and that is the point rather than a
//! convenience: membership in the roster is compile-time, so fetching is
//! disabled and the cache home redirected, and the five still come back in
//! their order.

use std::{env, fs};

use ganja_core::provider;

/// The five, in the order the seam must offer them. Spelled out rather than
/// imported from the constant: a test that read the same array it is checking
/// would pass however that array was reordered, and the order is half of what
/// was pinned.
const OFFERED: [&str; 5] =
    ["gpt-5.5", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.3-codex-spark"];

#[tokio::test]
async fn a_chatgpt_login_is_offered_the_pinned_five_and_no_other_credential_is_offered_anything() {
    let store = tempfile::tempdir().expect("a temp directory");
    let cache = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", store.path());
        env::set_var("XDG_CACHE_HOME", cache.path());
        env::set_var("GANJA_DISABLE_MODELS_FETCH", "1");
        env::remove_var("OPENAI_API_KEY");
    }

    // A machine with no openai credential at all: browsing the vendor's rows
    // is still useful, so the listing declines rather than refusing, and the
    // catalog stays the source of truth.
    assert!(
        provider::wire_model_listing("openai").await.is_none(),
        "logged out, openai is the catalog's to describe"
    );

    write_chatgpt_login(store.path());

    let listed = provider::wire_model_listing("openai")
        .await
        .expect("a stored ChatGPT login is a seat, and a seat has its own roster")
        .expect("the seat arm reaches nothing that could fail");

    let offered: Vec<&str> = listed.models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        offered, OFFERED,
        "the seat is offered exactly the pinned five, in the pinned order"
    );
    assert!(
        listed.notice.contains("pinned") && listed.notice.contains("--refresh"),
        "and the notice says so rather than claiming a live wire: {}",
        listed.notice
    );
    for model in &listed.models {
        assert!(
            !model.name.is_empty(),
            "a row the catalog cannot name is labelled by its id: {model:?}"
        );
    }

    // A key outranks a login for the same reason a request does: it is what
    // this session would authenticate with, and it reaches the platform
    // backend, which is held to no seat's offering.
    unsafe {
        env::set_var("OPENAI_API_KEY", "sk-not-a-real-key");
    }
    assert!(
        provider::wire_model_listing("openai").await.is_none(),
        "an API key session browses the catalog, seat or no seat stored beside it"
    );
}

/// A stored ChatGPT credential, in the shape `ganja auth login` writes one.
///
/// The tokens are inert strings: nothing on this path presents them, because
/// nothing on this path makes a request.
fn write_chatgpt_login(data_home: &std::path::Path) {
    let directory = data_home.join("ganja");
    fs::create_dir_all(&directory).expect("the store directory is creatable");
    let path = directory.join("auth.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "openai": {
                "type": "oauth",
                "refresh": "rt-seat-fixture",
                "access": "at-seat-fixture",
                "expires": 4_102_444_800_000_u64,
            }
        }))
        .expect("the fixture serializes"),
    )
    .expect("the fixture writes");

    // The store refuses a credential file other users can read, which is a
    // refusal this fixture would otherwise trip over.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("the fixture is made private");
    }
}
