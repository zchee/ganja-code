//! One real unary RPC against `api2.cursor.sh`, opted into by hand.
//!
//! `#[ignore]`d so no default run and no CI lane ever touches the network or
//! the real credential store: this binary exists for a person at a keyboard
//! re-verifying that the wire this build speaks is still the wire the live
//! probe recorded (`.omc/research/cursor/spike-wire-facts.md`). It reads the
//! stored cursor login through the same surface a session does, calls the
//! service's cheapest RPC, and asserts the answer decodes — nothing more.
//!
//! Run it with:
//!
//! ```sh
//! cargo nextest run -p ganja-provider --run-ignored only cursor_live
//! # or: cargo test -p ganja-provider --test cursor_live -- --ignored
//! ```
//!
//! Nothing here may render a token: the failure paths print the error types'
//! own messages, which the auth and provider layers already keep
//! secret-free, and the assertions print model ids and counts only.

use ganja_provider::provider::cursor::CursorWire;

#[tokio::test]
#[ignore = "talks to api2.cursor.sh with the stored cursor login; run explicitly with --ignored"]
async fn the_stored_login_is_answered_with_a_nonempty_model_list() {
    let wire = CursorWire::from_stored()
        .expect("a stored cursor login (run `ganja auth login cursor` first)");

    let models = wire
        .usable_models()
        .await
        .expect("the live listing answers and decodes");

    assert!(
        !models.is_empty(),
        "the live listing served no models at all"
    );
    // The `default` alias pair was live-observed; its absence would mean the
    // schema moved under this build.
    assert!(
        models
            .iter()
            .any(|model| model.model_id.as_deref() == Some("default")),
        "no `default` entry among {} models",
        models.len()
    );
}
