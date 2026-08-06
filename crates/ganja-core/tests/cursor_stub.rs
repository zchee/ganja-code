//! The cursor stub, from selection to its refusal.
//!
//! `cursor` is selectable — a shipped identity, not a typo — and everything
//! after selection is a named deferral: construction reads nothing, and the
//! first request is refused with the message that says what to do instead.
//! Everything here names the provider through the `--model cursor/…` override
//! tier rather than `GANJA_PROVIDER`, so no process-wide environment is
//! mutated and this binary can hold more than one test.

use ganja_core::{
    Config, Overrides,
    provider::{self, ChatRequest},
};
use tokio_util::sync::CancellationToken;

/// A config whose override names the stub and a model, the way `--model
/// cursor/…` would.
fn config() -> Config {
    Config {
        overrides: Overrides {
            model: Some("cursor/still-imaginary".to_owned()),
            ..Overrides::default()
        },
        ..Config::default()
    }
}

#[tokio::test]
async fn a_cursor_session_selects_and_is_refused_at_its_first_request() {
    let selection = provider::select(&config()).expect("the stub builds without reading anything");
    assert_eq!(selection.provider.id(), "cursor");
    assert_eq!(selection.model, "still-imaginary");
    assert!(
        selection.notice.is_none(),
        "the provider was asked for by name, not defaulted"
    );

    let refusal = selection
        .provider
        .stream(
            ChatRequest {
                model: selection.model,
                system: None,
                messages: Vec::new(),
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("a stub that streamed would be claiming a wire this build does not have");

    let rendered = refusal.to_string();
    assert!(rendered.contains("stub"), "{rendered}");
    assert!(
        rendered.contains("`provider` table"),
        "the refusal says what to do instead: {rendered}"
    );
}
