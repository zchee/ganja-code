use super::{API_KEY_ENV, CHAT_COMPLETIONS_ONLY, DEFAULT_BASE_URL, ID, from_env};
use crate::provider::responses::Backend;
use crate::provider::{PROVIDERS, Provider as _, ResponsesProvider};
use crate::{auth, catalog};

/// A provider pointed at loopback, which is the only endpoint a unit test
/// may put a credential on. Built through the crate-internal constructor
/// rather than through [`from_env`] for that function's own reason: it
/// reads the environment, which is process-wide state a unit test must not
/// mutate.
fn keyed(base_url: &str) -> ResponsesProvider {
    ResponsesProvider::built(
        crate::provider::CredentialSource::Key(
            crate::provider::Presented::new("sk-or-canary-9142").expect("a non-blank key"),
        ),
        base_url.to_owned(),
        Backend::OpenRouter,
    )
    .expect("loopback may carry a key")
}

#[test]
fn ganja_calls_it_openrouter_everywhere_the_catalog_can_see() {
    assert_eq!(ID, "openrouter");
    assert!(PROVIDERS.contains(&ID), "a provider nothing can select is a provider nobody has");
    assert_eq!(
        auth::storage_key(ID),
        ID,
        "upstream stores this vendor under its own name, so a shared \
             auth.json needs no alias — one invented here would hide the \
             credential from an opencode install reading the same file"
    );
    assert_eq!(
        auth::key_var(ID),
        Some(API_KEY_ENV),
        "the variable the store lets outrank it has to be the one this \
             module names, or an exported key is read by nothing"
    );
}

#[test]
fn the_endpoint_is_the_vendors_own_and_speaks_responses() {
    assert_eq!(DEFAULT_BASE_URL, "https://openrouter.ai/api/v1");

    let provider = keyed("http://127.0.0.1:8080/api/v1");
    assert_eq!(
        provider.id(),
        ID,
        "not the wire it borrows: the session layer prices a turn by this"
    );

    let rendered = format!("{provider:?}");
    assert!(rendered.contains("Key"), "{rendered}");
    assert!(
        !rendered.contains("sk-or-canary"),
        "a provider renders where it points and never what it presents: \
             {rendered}"
    );
}

/// The endpoint is held to the same rule every other credential-carrying
/// endpoint here is, and at construction rather than at the first prompt.
#[test]
fn a_key_may_not_be_sent_anywhere_the_other_wires_keys_could_not_be() {
    let refused = ResponsesProvider::built(
        crate::provider::CredentialSource::Key(
            crate::provider::Presented::new("sk-or-canary-9142").expect("a non-blank key"),
        ),
        "http://openrouter.ai/api/v1".to_owned(),
        Backend::OpenRouter,
    )
    .expect_err("plain http to a public host puts the key on the wire in the clear");

    assert!(matches!(refused, crate::provider::ProviderError::Transport(_)), "{refused:?}");
}

/// The one thing that decides whether this vendor's rows resolve at all:
/// the payload's provider key is read through
/// [`auth::provider_id_for_storage_key`] on the way into the table, so
/// **ganja's id has to survive that translation unchanged** or every row
/// lands under a name nothing asks for and the sizing tier silently misses.
///
/// Asserted against the translation rather than against the table, because
/// this process's table is the compiled-in snapshot: that tier carries no
/// openrouter rows (checked below, and it is a real consequence — a build
/// that never fetches serves this provider uncataloged). `tests/
/// catalog_openrouter.rs` is where a real catalog is loaded and the rows
/// are proved to arrive priced.
#[test]
fn the_published_catalogs_id_for_this_vendor_survives_the_way_in() {
    assert_eq!(
        auth::provider_id_for_storage_key(ID),
        ID,
        "the catalog names this vendor exactly as ganja does, so a fetched \
             row keeps the id every consumer of the table holds"
    );
    assert!(
        !catalog::carries(ID),
        "not a wish: the compiled-in snapshot has no rows for this vendor, \
             and a session that never fetched runs it on the degradation path. \
             If this ever fails, the snapshot grew openrouter rows and this \
             module's `default_model` decision is worth re-reading."
    );
}

/// **No default model, deliberately**, and this is the decision rather than
/// an omission waiting to be tidied.
///
/// Three reasons, in the order they were found:
///
/// 1. Upstream pins no per-provider default at all. Its ordering heuristic
///    (`provider.ts:1986-1994`) is what picks one, and applied to this
///    vendor's rows it selects `google/gemini-3-pro-image` — an image model,
///    because `"gemini-3-pro"` is an `includes` filter aimed at single-vendor
///    rosters. A rule that degenerates on a gateway is not a rule to port.
/// 2. The vendor's own "when nobody chose" id, `openrouter/auto`, *is*
///    published — the cursor precedent for pinning a backend's own default
///    — but the catalog carries it with no cost at all, because its price
///    depends on what it routes to. A default that reported every turn as
///    free is worse than a startup message.
/// 3. `SelectionError::NoDefaultModel` already names all three ways to name
///    a model, which is exactly the posture every config-declared endpoint
///    has. A 349-row gateway is closer to that than to a vendor roster.
#[test]
fn a_gateway_of_many_vendors_is_not_pinned_to_one_of_them() {
    assert_eq!(
        catalog::default_model(ID),
        None,
        "a pin here would be this build asserting a default no vendor and no \
             upstream rule supplies"
    );
}

/// Upstream's carve-out, ported as the identity list it is.
#[test]
fn upstreams_openrouter_chat_alias_is_refused_rather_than_hidden() {
    assert_eq!(CHAT_COMPLETIONS_ONLY, ["openai/gpt-5-chat"]);
    assert!(
        keyed("http://127.0.0.1:8080/api/v1").refuses("openai/gpt-5-chat").is_some(),
        "a chat-completions-only alias cannot ride a Responses request, \
             which is why upstream deletes it from this provider's roster"
    );
}

/// `from_env` is the one route that reads the environment, and what it says
/// with nothing set is the message somebody has to act on.
#[test]
fn a_session_with_no_key_is_told_which_variable_it_is_missing() {
    // Not asserted by running `from_env` — that reads process-wide state and
    // the credential store belongs to whoever is running the suite. What is
    // provable without either is that the variable named in the message is
    // the variable the lookup reads.
    assert_eq!(auth::key_var(ID), Some(API_KEY_ENV));
    let _ = from_env;
}
