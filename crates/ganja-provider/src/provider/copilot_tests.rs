use secrecy::SecretString;

use super::{
    API_VERSION_HEADER, CopilotProvider, ID, INITIATOR, INITIATOR_HEADER, INTENT, INTENT_HEADER,
    NeverRenews, headers,
};
use crate::{
    auth::{self, OauthCredential, RefreshOauth as _},
    catalog,
    provider::{PROVIDERS, Provider as _, ProviderError},
};

#[test]
fn one_name_reaches_the_login_the_catalog_and_the_command_line() {
    assert_eq!(ID, "github-copilot");
    assert_eq!(
        ID,
        auth::copilot::PROVIDER_ID,
        "one constant, or a login stores under a name the provider does not read"
    );
    assert!(
        PROVIDERS.contains(&ID),
        "a provider nothing can select is a provider nobody has"
    );
    // Unlike grok, whose file key is upstream's `xai`, this provider is
    // spelled the same everywhere — so there is no alias to look through,
    // and asserting that is what would catch one being added by accident.
    assert_eq!(
        auth::storage_key(ID),
        ID,
        "upstream calls this provider `github-copilot` too; an alias here \
             would be a second name for one thing"
    );
}

#[test]
fn the_endpoint_is_githubs_own_and_speaks_chat_completions() {
    let provider = CopilotProvider::from_stored().expect("a client builds");

    assert_eq!(provider.id(), ID, "not the wire it borrows");

    // A provider renders as which provider it is and where it points, and
    // never as what it authenticates with — this one has nothing to render
    // yet, because the token is not fetched until a request needs it.
    let rendered = format!("{provider:?}");
    assert!(
        rendered.contains("Oauth") && rendered.contains(ID),
        "{rendered}"
    );
    assert!(
        rendered.contains(auth::copilot::DEFAULT_API_BASE),
        "the endpoint is what tells one provider from another: {rendered}"
    );
}

/// The token is not exempt from the rule every other provider's base URL is
/// held to just because the credential arrived from a device flow.
#[test]
fn a_github_token_may_not_be_sent_anywhere_a_key_could_not_be() {
    let refused = CopilotProvider::at("http://api.githubcopilot.com")
        .expect_err("plain http to a public host puts the token on the wire in the clear");

    assert!(
        matches!(refused, ProviderError::Transport(_)),
        "{refused:?}"
    );
    assert!(
        CopilotProvider::at("http://127.0.0.1:8080").is_ok(),
        "loopback never reaches a network, which is what a test depends on"
    );
}

/// The four headers, as constants rather than as a request — the request
/// itself is asserted over a socket in `tests/copilot_wire.rs`, and this is
/// the half that says a change to one of these values was somebody's
/// decision. Every one of them was confirmed against the live endpoint
/// together, so none of them is a value to adjust on a hunch.
#[test]
fn the_four_headers_are_the_ones_the_endpoint_was_measured_with() {
    let headers = headers();
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_else(|| panic!("{name} should be sent on every request"))
    };

    assert_eq!(headers.len(), 4, "four, and nothing that crept in beside");
    assert_eq!(value("user-agent"), auth::device::UPSTREAM_USER_AGENT);
    assert_eq!(value("user-agent"), "opencode/1.18.22");
    assert_eq!(value(API_VERSION_HEADER), auth::copilot::API_VERSION);
    // Asserted as a literal as well as through the constant, so that moving
    // the date is a decision somebody has to come here and confirm.
    assert_eq!(value(API_VERSION_HEADER), "2026-06-01");
    assert_eq!(value(INTENT_HEADER), INTENT);
    assert_eq!(value(INTENT_HEADER), "conversation-edits");
    assert_eq!(value(INITIATOR_HEADER), INITIATOR);
    assert_eq!(value(INITIATOR_HEADER), "user");
    assert!(
        headers.get("authorization").is_none(),
        "the bearer is resolved per request and never held in a header set"
    );
}

/// The renewal that cannot happen still has to answer sensibly if it ever
/// does. A refusal is what a caller can act on — only another login repairs
/// this credential — and it is classified as one, which keeps the retry
/// driver from hammering an endpoint that does not exist.
#[tokio::test]
async fn a_renewal_that_has_no_endpoint_refuses_rather_than_panicking() {
    let credential = OauthCredential::new(
        SecretString::from("gho_refresh-canary"),
        SecretString::from("gho_access-canary"),
        0,
    );
    let refused = NeverRenews
        .refresh(ID, &credential)
        .await
        .expect_err("there is no endpoint that could have answered");

    assert_eq!(refused.kind(), auth::AuthErrorKind::ReauthRequired);
    assert!(
        format!("{refused}").contains(&format!("ganja auth login {ID}")),
        "the message is what a status bar shows, and only a login fixes \
             this: {refused}"
    );
}

/// The other half of the obligation `catalog`'s own table test states — and
/// the one place where this provider's answer differs from every other
/// provider's: the row is deliberately unpriced, because a subscription
/// seat has no per-token price to report. Sizing still has to be there, or
/// a session has no context window to compact against.
#[test]
fn a_copilot_session_gets_a_model_the_catalog_can_size_and_deliberately_not_price() {
    let id = catalog::default_model(ID).expect("copilot has a pinned default");
    let info = catalog::model(id).expect("the default is in the table");

    assert_eq!(info.provider_id, ID);
    // GitHub's limits, not the model's. The same model served by Anthropic
    // directly takes a million tokens; this proxy resells a fifth of that,
    // and a session sized to the larger figure stops compacting and starts
    // being refused. Exact rather than `> 0`, because the plausible wrong
    // answer here is a *bigger* number.
    assert_eq!(
        (info.context_window, info.max_output),
        (200_000, 64_000),
        "a Copilot window is the proxy's, and it is not the model's"
    );
    assert_eq!(
        (
            info.pricing.input,
            info.pricing.output,
            info.pricing.cache_read
        ),
        (0.0, 0.0, 0.0),
        "a seat is billed by the month; a per-token figure here would be \
             invented rather than reported"
    );
    assert_eq!(info.pricing.cache_write, None);
}
