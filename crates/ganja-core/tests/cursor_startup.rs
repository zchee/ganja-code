//! A cursor session, from selection to its first request, with no login.
//!
//! `cursor` is selectable — a shipped identity, not a typo — and construction
//! reads nothing, so selection succeeds on a machine that has never logged
//! in. What meets the first request is the wire reading the stored login at
//! the moment it is needed and refusing by name when there is none: the
//! message says `ganja auth login cursor`, because that is the repair.
//!
//! One test, one binary, on purpose: it redirects `XDG_DATA_HOME` so the
//! wire reads an empty credential store rather than whatever this machine
//! really holds — without the redirect this test would either touch the
//! network or change verdict with the developer's logins — and a plain
//! `cargo test` runs a binary's tests on parallel threads.

use std::env;

use ganja_core::provider::{self, ChatRequest, ProviderError};
use ganja_core::{Config, Overrides};
use tokio_util::sync::CancellationToken;

/// A config whose override names cursor and a model, the way `--model
/// cursor/…` would.
fn config() -> Config {
    Config {
        overrides: Overrides {
            model: Some("cursor/gpt-5.3-codex".to_owned()),
            ..Overrides::default()
        },
        ..Config::default()
    }
}

#[tokio::test]
async fn a_cursor_session_without_a_login_is_refused_naming_the_login() {
    let store = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", store.path());
    }

    let selection = provider::select(&config()).expect("selection reads nothing and succeeds");
    assert_eq!(selection.provider.id(), "cursor");
    assert_eq!(selection.model, "gpt-5.3-codex");
    assert!(selection.notice.is_none(), "the provider was asked for by name, not defaulted");

    let refused = selection
        .provider
        .stream(
            ChatRequest {
                effort_options: Default::default(),
                model: selection.model,
                system: None,
                messages: Vec::new(),
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("a first request with no stored login is refused, not sent");

    assert!(matches!(refused, ProviderError::Auth(_)), "{refused:?}");
    let rendered = refused.to_string();
    assert!(
        rendered.contains("ganja auth login cursor"),
        "the refusal says what to do next: {rendered}"
    );
}
