//! The wire model listing, asked about cursor on a machine with no login.
//!
//! The seam's positive arm reads the credential store before it dials, so
//! with nothing stored the refusal is the wire's own `Auth` error — naming
//! `ganja auth login`, which is the repair — and no network is needed to
//! prove it.
//!
//! One test, one binary, on purpose: it redirects `XDG_DATA_HOME` so the
//! seam reads an empty credential store rather than whatever this machine
//! really holds — without the redirect this test would either touch the
//! network or change verdict with the developer's logins — and a plain
//! `cargo test` runs a binary's tests on parallel threads.

use std::env;

use ganja_core::provider::{self, ProviderError};

#[tokio::test]
async fn the_cursor_listing_without_a_login_is_refused_naming_the_login() {
    let store = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", store.path());
    }

    // The literal id rather than `provider::cursor::ID`: the seam's callers
    // hand it whatever a person typed, so the public spelling is the thing
    // to pin.
    let answer = provider::wire_model_listing("cursor")
        .await
        .expect("cursor is the one provider the wire listing answers for");
    let refused = answer.expect_err("an empty store refuses before anything is dialled");

    assert!(matches!(refused, ProviderError::Auth(_)), "{refused:?}");
    let rendered = refused.to_string();
    assert!(
        rendered.contains("ganja auth login cursor"),
        "the refusal says what to do next: {rendered}"
    );
}
