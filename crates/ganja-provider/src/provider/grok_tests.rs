use std::sync::Arc;

use super::{DEFAULT_BASE_URL, GrokProvider, ID};
use crate::{
    auth::{self, AuthError, OauthCredential, RefreshOauth},
    catalog,
    provider::{PROVIDERS, Provider as _, ProviderError},
};

/// A renewal that must never run, for the cases that are about construction
/// rather than about a token endpoint.
struct NeverRenews;

#[async_trait::async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        panic!("{provider_id} was renewed by a test that only builds a provider");
    }
}

#[test]
fn ganja_calls_it_grok_everywhere_the_wire_can_see() {
    assert_eq!(ID, "grok");
    assert_eq!(
        ID,
        auth::grok::PROVIDER_ID,
        "one constant, or a login stores under a name the provider does not read"
    );
    assert!(
        PROVIDERS.contains(&ID),
        "a provider nothing can select is a provider nobody has"
    );
    // What the credential file calls this provider is deliberately not
    // written down anywhere in `provider/`, not even in an assertion:
    // `auth::storage_key` owns that translation and `auth::grok`'s own
    // tests pin it. A second spelling here would be a second opinion about
    // where the credential lives.
    assert_ne!(
        auth::storage_key(ID),
        ID,
        "the store's name for this provider is not ganja's, and only `auth` knows it"
    );
}

#[test]
fn the_endpoint_is_xais_own_and_speaks_chat_completions() {
    assert_eq!(DEFAULT_BASE_URL, "https://api.x.ai/v1");

    // Built through `at` at the same constant `from_stored` passes, rather
    // than through `from_stored` itself: that route now asks the credential
    // store whether there is a login, and the store belongs to whoever is
    // running the suite. What this test is about — the endpoint and the id
    // — is the same either way, and `tests/grok_startup.rs` is where the
    // probe is drilled, against an `XDG_DATA_HOME` it owns.
    let provider =
        GrokProvider::at(DEFAULT_BASE_URL, Arc::new(NeverRenews)).expect("a client builds");
    assert_eq!(provider.id(), ID, "not the wire it borrows");

    // A provider renders as which provider it is and where it points, and
    // never as what it authenticates with — this one has nothing to render
    // yet, because the token is not fetched until a request needs it.
    let rendered = format!("{provider:?}");
    assert!(
        rendered.contains("Oauth") && rendered.contains("grok"),
        "{rendered}"
    );
    assert!(
        rendered.contains("https://api.x.ai/v1"),
        "the endpoint is what tells one provider from another: {rendered}"
    );
}

/// The endpoint is not exempt from the rule the base URL of every other
/// provider is held to just because the credential arrived as a token
/// rather than as a key.
#[test]
fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
    let refused = GrokProvider::at("http://api.x.ai/v1", Arc::new(NeverRenews))
        .expect_err("plain http to a public host puts the token on the wire in the clear");

    assert!(
        matches!(refused, ProviderError::Transport(_)),
        "{refused:?}"
    );
    assert!(
        GrokProvider::at("http://127.0.0.1:8080/v1", Arc::new(NeverRenews)).is_ok(),
        "loopback never reaches a network, which is what a test depends on"
    );
}

/// The other half of the obligation `catalog`'s own table test states: a
/// provider a session can select has to be one the catalog can size and
/// price, or the first turn has no model to ask for and no cost to report.
#[test]
fn a_grok_session_that_names_no_model_gets_one_the_catalog_can_price() {
    let id = catalog::default_model(ID).expect("grok has a pinned default");
    let info = catalog::model(id).expect("the default is in the table");

    assert_eq!(info.provider_id, ID);
    assert!(info.context_window > 0 && info.max_output > 0);
    assert!(
        info.pricing.input > 0.0 && info.pricing.output > 0.0,
        "a priced provider with a free row is a row nobody filled in"
    );
}
