//! What a stored credential can and cannot do to the wire model listing.
//!
//! Since **D555** the seam's answer is a fact about the **provider id**, not
//! about the credential: `chatgpt` is offered the pinned six and `openai` is
//! the catalog's to describe, whatever either one has stored beside it. That
//! makes the listing itself a crate-local test (`provider_tests.rs`), and
//! leaves this binary the half that needs a real store to say anything at all —
//! the **negative** claim that a login does not move either answer.
//!
//! It is the same claim `wire_lists_models`'s doc used to have to hedge about,
//! now stated where a store exists to contradict it: a machine holding a
//! pre-split ChatGPT login under `openai` gets exactly the answers a machine
//! holding nothing does, because nothing on this path reads the file any more.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and a plain `cargo test` runs a binary's tests on parallel
//! threads.
//!
//! Nothing here reaches the network, and that is the point rather than a
//! convenience: membership in the roster is compile-time, so fetching is
//! disabled and the cache home redirected, and the six still come back in
//! their order.

use std::{env, fs};

use ganja_core::provider;

/// The six, in the order the seam must offer them. Spelled out rather than
/// imported from the constant: a test that read the same array it is checking
/// would pass however that array was reordered, and the order is half of what
/// was pinned.
const OFFERED: [&str; 6] = [
    "gpt-6-astra",
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.3-codex-spark",
];

#[tokio::test]
async fn the_seat_lists_the_pinned_six_and_no_stored_credential_moves_either_id() {
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

    // An empty store. The seat is offered its roster — a listing has to stay
    // usable logged out, and after D555 there is nothing in the way of that —
    // and the platform is the catalog's to describe.
    assert_offering().await;

    // A ChatGPT login written before the split, sitting under `openai`. The
    // one arrangement that could still produce a fallback read, and neither
    // answer moves: `chatgpt` did not need it, and `openai` must not find it.
    write_pre_split_chatgpt_login(store.path());
    assert_offering().await;

    // And a platform key beside it, which is what used to decide the whole
    // question.
    // SAFETY: as above.
    unsafe {
        env::set_var("OPENAI_API_KEY", "sk-not-a-real-key");
    }
    assert_offering().await;
}

/// The two answers, asserted the same way whatever the store holds.
async fn assert_offering() {
    assert!(
        provider::wire_model_listing("openai").await.is_none(),
        "the platform is the catalog's to describe, seat or no seat stored beside it"
    );

    let listed = provider::wire_model_listing("chatgpt")
        .await
        .expect("the seat answers for its own id")
        .expect("the seat arm reaches nothing that could fail");

    let offered: Vec<&str> = listed.models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(offered, OFFERED, "the seat is offered exactly the pinned six, in the pinned order");
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
}

/// A ChatGPT credential filed under `openai`, the way `ganja auth login` wrote
/// one before **D555** moved the seat to its own key.
///
/// The tokens are inert strings: nothing on this path presents them, because
/// nothing on this path makes a request — and nothing on this path reads them
/// either, which is the thing being pinned.
fn write_pre_split_chatgpt_login(data_home: &std::path::Path) {
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
