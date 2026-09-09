use std::collections::BTreeMap;

use super::{
    Config, Dialect, PROVIDER_ENV, PROVIDERS, ProviderConfig, SelectionError, ToolReach,
    adoptable_login, cursor, defaulted_model, fake, grok, openai, opencode, openrouter, responses,
    select, selectable, wire_lists_models, wire_model_listing,
};
use crate::auth::CredentialKind;
use crate::catalog;

/// Storage keys as [`adoptable_login`] takes them, every one of them a stored
/// **key**.
///
/// The kind matters to exactly one of its filters, and the tests that exercise
/// that filter spell their pairs out; everywhere else a key is the ordinary
/// entry and saying so once beats repeating it per row.
fn stored(keys: &[&str]) -> Vec<(String, CredentialKind)> {
    keys.iter().map(|key| ((*key).to_owned(), CredentialKind::ApiKey)).collect()
}

/// A config declaring one endpoint under `id`.
fn declaring(id: &str) -> Config {
    let mut config = Config::default();
    config.provider.insert(
        id.to_owned(),
        ProviderConfig {
            dialect: Dialect::OpenaiChatCompletions,
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            key_env: None,
            headers: BTreeMap::new(),
        },
    );

    config
}

/// The two tiers, at the boundary that separates them. A session may run
/// as anything in either, and the catalog knows only some of it — so
/// neither predicate may be derived from the other.
#[test]
fn a_config_named_provider_is_selectable_and_a_builtin_is_not_always_cataloged() {
    let config = declaring("local-llama");

    assert!(selectable(&config, "local-llama"));
    assert!(
        !PROVIDERS.contains(&"local-llama"),
        "the config tier is what makes it selectable, not the shipped list"
    );
    assert!(!catalog::carries("local-llama"), "no published catalog knows a private endpoint");

    for builtin in PROVIDERS {
        assert!(
            selectable(&config, builtin),
            "{builtin} ships, so it is selectable whatever a config says"
        );
    }
    // The tier boundary inside the builtins themselves: `fake` is
    // selectable and deliberately uncataloged, and `cursor` — the wire
    // that landed before its rows, exactly the shape this comment used to
    // predict — rides the same tier until the real wire brings its rows.
    assert!(!catalog::carries(fake::ID));
    assert!(!catalog::carries(cursor::ID));
    assert!(catalog::carries(openai::ID));

    assert!(!selectable(&config, "gemini"));
    assert!(!selectable(&Config::default(), "local-llama"));
}

/// The gateways sit in a tier of their own, and both halves of that are
/// deliberate: they ship, so nothing has to be configured to reach them,
/// and they are pinned to no model, so a session has to say which of the
/// vendors they front it wants.
///
/// The second half is the one worth a test of its own. Every other builtin
/// either has a catalog pin or is uncataloged on purpose; this one is fully
/// sized and priced and still refuses to guess, so the refusal has to be
/// the actionable kind — see `ganja_provider::provider::openrouter` for the
/// three reasons the pin is absent.
#[test]
fn a_gateway_ships_selectable_and_pinned_to_none_of_the_vendors_it_fronts() {
    for gateway in [openrouter::ID, opencode::ZEN_ID, opencode::GO_ID] {
        assert!(
            selectable(&Config::default(), gateway) && PROVIDERS.contains(&gateway),
            "{gateway} ships"
        );

        let refused = defaulted_model(gateway, None)
            .expect_err("no pin, so nothing may be substituted for a choice");
        let rendered = refused.to_string();
        assert!(rendered.contains(gateway), "{rendered}");
        for way in ["GANJA_MODEL", "--model", "`model`"] {
            assert!(
                rendered.contains(way),
                "a refusal that does not say how to answer it is a dead end: \
                     {way} missing from {rendered}"
            );
        }
    }

    // Not the fake provider's arm, and not a wire default either: those two
    // are what `defaulted_model` answers *before* it asks the catalog, and
    // the whole point here is that it asked and the catalog had nothing.
    assert_eq!(
        defaulted_model(openrouter::ID, Some("openai/gpt-5.4"))
            .expect("a wire that names its own default is answered"),
        "openai/gpt-5.4",
        "the seam is still there for a backend that ever grows one"
    );
}

/// A refusal that listed only the shipped providers would tell somebody
/// who had just declared an endpoint that their own entry does not exist,
/// which is the one answer that cannot be acted on — and one that did not
/// say which tier asked would send them to unset a variable a config key
/// set.
#[test]
fn the_refusal_for_an_unknown_provider_names_both_tiers_and_who_asked() {
    let named = SelectionError::Unknown {
        requested: "gemini".to_owned(),
        named_by: "the config's `default_provider` key",
        configured: vec!["local-llama".to_owned(), "gateway".to_owned()],
    };
    let rendered = named.to_string();

    assert!(rendered.contains("gemini"), "{rendered}");
    assert!(
        rendered.contains("default_provider"),
        "the tier that named the id is the thing to fix: {rendered}"
    );
    for builtin in PROVIDERS {
        assert!(rendered.contains(builtin), "{builtin} is missing: {rendered}");
    }
    assert!(
        rendered.contains("local-llama") && rendered.contains("gateway"),
        "the config's own endpoints are as selectable as the builtins: {rendered}"
    );

    // A session with no such table gets the message it always had, rather
    // than one carrying an empty list.
    let bare = SelectionError::Unknown {
        requested: "gemini".to_owned(),
        named_by: PROVIDER_ENV,
        configured: Vec::new(),
    }
    .to_string();
    assert!(bare.contains(PROVIDER_ENV), "the environment tier is named as itself: {bare}");
    assert!(
        !bare.contains("this config names"),
        "nothing was configured, so nothing should be listed: {bare}"
    );
}

/// The login tier's own rule, without a credential store: the oldest
/// login this session can actually run as, in ganja's vocabulary.
#[test]
fn the_oldest_login_that_wins_is_the_oldest_one_this_session_can_run_as() {
    // The file speaks upstream's names: an `xai` login is a grok session.
    assert_eq!(
        adoptable_login(&Config::default(), stored(&["xai", "anthropic"])).as_deref(),
        Some(grok::ID)
    );

    // A login this build has no wire for is skipped, not refused — the
    // file may be shared with opencode, whose logins are its own.
    assert_eq!(
        adoptable_login(&Config::default(), stored(&["gemini", "anthropic"])).as_deref(),
        Some("anthropic")
    );

    // Unless a config declares that very endpoint, which makes its stored
    // login as runnable as a builtin's.
    assert_eq!(
        adoptable_login(&declaring("gemini"), stored(&["gemini", "anthropic"])).as_deref(),
        Some("gemini")
    );

    // A credential filed under the fake id is not a login to anything,
    // and the fake fallback must keep its notice.
    assert_eq!(adoptable_login(&Config::default(), stored(&[fake::ID])), None);
    assert_eq!(adoptable_login(&Config::default(), stored(&[])), None);
}

/// **AC-0.4** and **AC-0.11**: the filter **D555** added, stated the way every
/// other one here is — over pairs, with no store anywhere in sight.
///
/// A ChatGPT login written before the split sits under `openai`, an id that now
/// means the platform API and a key. Adopting it would build a wire that
/// refuses that credential at its first request, so it is skipped and the
/// session falls through — to the next adoptable login, or to the fake provider
/// with the notice that tier exists to produce. The credential is not moved,
/// rewritten or deleted: `ganja auth login chatgpt` is the repair, and a
/// warning is where it is offered.
///
/// The **kind** is what the rule turns on, which is why both `openai` rows are
/// here: a stored platform key under the same id is a perfectly good login and
/// adopts exactly as it always did.
#[test]
fn a_pre_split_chatgpt_login_under_the_platform_id_is_never_adopted() {
    let oauth = |key: &str| vec![(key.to_owned(), CredentialKind::Oauth)];

    assert_eq!(
        adoptable_login(&Config::default(), oauth(openai::ID)),
        None,
        "the only entry is a login no wire reads, so this machine is the fake \
             provider's — with its notice"
    );
    assert_eq!(
        adoptable_login(&Config::default(), stored(&[openai::ID])).as_deref(),
        Some(openai::ID),
        "a stored *key* under that id is the platform's own credential"
    );

    // Skipped, never fatal: the next login this session can run as still wins.
    let mixed =
        vec![(openai::ID.to_owned(), CredentialKind::Oauth), stored(&["anthropic"])[0].clone()];
    assert_eq!(adoptable_login(&Config::default(), mixed).as_deref(), Some("anthropic"));

    // And the seat's own id is adopted like anybody else's login — the skip is
    // about the *pre-split* key, not about ChatGPT.
    assert_eq!(
        adoptable_login(&Config::default(), oauth(responses::CHATGPT_ID)).as_deref(),
        Some(responses::CHATGPT_ID)
    );
}

/// The flip the stub-era filter's own comment promised: with the wire
/// real, a cursor login adopts like any other stored login — seniority
/// decides, in both directions, and a machine holding only a cursor
/// login runs as it rather than as the noticed fake. The other side of
/// the decision — naming cursor explicitly — is pinned unchanged below.
#[test]
fn a_cursor_login_adopts_like_any_other_stored_login() {
    // Oldest wins when cursor is oldest…
    assert_eq!(
        adoptable_login(&Config::default(), stored(&["cursor", "anthropic"])).as_deref(),
        Some("cursor")
    );
    // …and does not win when it is not: adoption is the ordering's
    // verdict, never a preference for the newest wire.
    assert_eq!(
        adoptable_login(&Config::default(), stored(&["anthropic", "cursor"])).as_deref(),
        Some("anthropic")
    );
    assert_eq!(adoptable_login(&Config::default(), stored(&["cursor"])).as_deref(), Some("cursor"));
    // Adopted or named, the id answers the same way everywhere else.
    assert!(selectable(&Config::default(), cursor::ID));
}

/// The other side of the same decision: naming cursor is answered, not
/// filtered — selection builds the wire's identity without reading
/// anything, and the named model travels through untouched. What the
/// first request then meets — the stored login, or the refusal naming
/// `ganja auth login` — is drilled in `ganja-provider`'s own suites
/// against a credential store they redirect; streaming here would read
/// whatever store the machine running this test really holds.
#[test]
fn an_explicitly_named_cursor_is_answered_not_filtered() {
    // The flag tier, because it outranks every other and so cannot be
    // perturbed by whatever this process's environment holds.
    let mut config = Config::default();
    config.overrides.model = Some("cursor/gpt-5.3-codex".to_owned());

    let selection = select(&config).expect("an explicit cursor selection is not filtered");
    assert_eq!(selection.provider.id(), cursor::ID);
    assert_eq!(selection.model, "gpt-5.3-codex");
    assert!(selection.notice.is_none(), "the provider was asked for by name, not defaulted");
}

/// The listing seam's whole negative half: for cataloged builtins, the fake
/// provider, config-declared endpoints and outright typos the answer is
/// [`None`] before anything is read or dialled, and the catalog stays the
/// source of truth.
///
/// `openai` joined this list with **D555**. Under **D476** it could not be
/// here — its answer read the environment and the credential store, so pinning
/// it would have made this test's verdict the developer's own logins — and now
/// the platform is simply the catalog's to describe, whatever is stored beside
/// it. That last clause is pinned out in `tests/`, by
/// `the_seat_lists_the_pinned_six_and_no_stored_credential_moves_either_id`,
/// which redirects a store so there is one to contradict; cursor's positive
/// half lives in `tests/cursor_models_listing.rs`.
#[tokio::test]
async fn the_wire_listing_answers_none_where_the_catalog_is_the_source_of_truth() {
    for provider in
        ["anthropic", openai::ID, grok::ID, fake::ID, "local-llama", "a-provider-nothing-ships"]
    {
        assert!(
            wire_model_listing(provider).await.is_none(),
            "{provider} is the catalog's to describe, not a wire's"
        );
    }
}

/// **AC-0.7.** The seat's own half of that seam, which since **D555** is a
/// crate-local test rather than an integration one: the id is the whole
/// question, so there is no store to redirect and no environment to hold.
///
/// The six and their order are the wire's (`responses::SEAT_ROSTER`), and are
/// pinned there rather than here — what this asserts is that the seam reaches
/// them at all, and that a row the catalog cannot name is still labelled.
#[tokio::test]
async fn the_seat_lists_its_own_roster_without_reading_a_credential() {
    assert!(wire_lists_models(responses::CHATGPT_ID));

    let listed = wire_model_listing(responses::CHATGPT_ID)
        .await
        .expect("the seat answers for itself")
        .expect("and the seat arm reaches nothing that could fail");
    let offered: Vec<&str> = listed.models.iter().map(|model| model.id.as_str()).collect();

    assert_eq!(offered, responses::SEAT_ROSTER);
    for model in &listed.models {
        assert!(!model.name.is_empty(), "a row the catalog cannot name is labelled by its id");
    }
}

/// A model no catalog carries, so an answer naming it can only have come
/// from the wire.
const SENTINEL: &str = "a-model-the-catalog-has-never-heard-of";

/// A backend that serves a narrower set than its vendor's catalog row has
/// to be able to say so, and this is the precedence that lets it.
///
/// The sentinel is what makes this a real assertion rather than a
/// coincidence: `openai`'s two defaults currently name the same model, so
/// comparing the strings would pass whether or not the wire is consulted
/// at all. Handing it a value the catalog could not produce is the only way
/// to tell "the wire decided" from "the table did" until the two diverge.
#[test]
fn a_backends_own_default_outranks_its_vendors_catalog_row() {
    assert!(
        catalog::model(SENTINEL).is_none(),
        "the sentinel has to be a model no table could have answered with"
    );
    assert_eq!(
        defaulted_model(openai::ID, Some(SENTINEL)).expect("a wire default needs no table"),
        SENTINEL,
        "a session on a backend that named its own default got the vendor's \
             row instead, which is how a ChatGPT seat ends up asking for a model \
             its backend refuses"
    );

    // Naming nothing falls through to the catalog, which is every other
    // provider and the openai key wire.
    assert_eq!(
        defaulted_model(openai::ID, None).expect("openai has a pinned default"),
        catalog::default_model(openai::ID).expect("openai has a pinned default")
    );
    // The fake provider is deliberately in no catalog: nothing canned has a
    // price, so it answers ahead of both.
    assert_eq!(
        defaulted_model(fake::ID, None).expect("the fake provider carries its own"),
        fake::MODEL
    );
    assert!(matches!(
        defaulted_model("nonexistent", None),
        Err(SelectionError::NoDefaultModel { .. })
    ));
    // Cursor left that refusal when the catalog pinned it: uncataloged —
    // no sizing, no pricing — yet defaulted, because the id the pin names
    // is the server-side Auto its own wire publishes.
    assert_eq!(
        defaulted_model(cursor::ID, None).expect("cursor has the wire-published pin"),
        "default"
    );
}

/// **AC-4**, inverted by **D552**: with cursor's bridge landed there is no
/// shipped id left that reaches less than every tool.
///
/// Asked of the id list rather than of nine constructed wires: the fact is a
/// function of the id, and instantiating a provider to ask it would need
/// credentials for eight vendors to answer a question about a string. The
/// `contains` guard is what makes the loop about cursor — the id this assertion
/// is about — since a loop that silently stopped including it would still pass.
#[test]
fn every_shipped_id_reaches_every_tool_this_build_registers() {
    for builtin in PROVIDERS {
        assert_eq!(
            ToolReach::of(builtin),
            ToolReach::Full,
            "{builtin} calls tools ganja registered, so every one of them is callable"
        );
    }
    assert!(PROVIDERS.contains(&cursor::ID), "and the loop above really did cover cursor");
}
