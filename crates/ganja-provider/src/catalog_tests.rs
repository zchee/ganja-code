use std::fs;
use std::sync::Arc;
use std::time::Duration;

use super::{
    Cost, DEFAULT_SOURCE, ModelStatus, Pricing, Source, backoff, cache_name, carries,
    compact_tokens, cost, default_model, fresh, model, parse, read_cached, scattered, snapshot,
    write_cache,
};
use crate::protocol::Usage;

/// Dollar amounts are compared with a tolerance because the arithmetic is
/// binary floating point; a tenth of a cent is far below anything shown.
fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

/// A payload in the shape the catalog publishes: providers keyed by id,
/// each holding models keyed by id.
fn payload() -> String {
    r#"{
          "fixture": {
            "id": "fixture",
            "name": "Fixture Inc",
            "env": ["FIXTURE_API_KEY"],
            "npm": "@fixture/sdk",
            "models": {
              "fixture-large": {
                "id": "fixture-large",
                "name": "Fixture Large",
                "family": "fixture",
                "release_date": "2026-01-31",
                "attachment": true,
                "reasoning": true,
                "temperature": false,
                "tool_call": true,
                "status": "beta",
                "modalities": { "input": ["text", "image"], "output": ["text"] },
                "reasoning_options": [{ "type": "toggle" }],
                "a_field_this_build_has_never_heard_of": { "nested": [1, 2, 3] },
                "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.4, "cache_write": 5.0 },
                "limit": { "context": 500000, "input": 400000, "output": 32000 },
                "variants": {
                  "max": { "thinking": { "type": "enabled", "budgetTokens": 32000 } },
                  "mini": { "reasoningEffort": "low" }
                }
              },
              "fixture-small": {
                "id": "fixture-small",
                "limit": { "context": 128000, "output": 8000 }
              },
              "fixture-other-transport": {
                "id": "fixture-other-transport",
                "provider": { "npm": "@fixture/other", "api": "https://elsewhere.example" },
                "limit": { "context": 128000, "output": 8000 }
              },
              "fixture-unsized": {
                "id": "fixture-unsized",
                "cost": { "input": 1.0, "output": 2.0 }
              }
            }
          }
        }"#
    .to_owned()
}

/// Providers whose turns are paid for by the month rather than by the
/// token.
///
/// A row under one of these carries no price at all, and that is the
/// honest figure rather than a hole — see the Copilot row for the whole
/// reasoning. Named as a provider, deliberately: a `price == 0.0` escape
/// would excuse every row anybody forgot to fill in, whereas this excuses
/// exactly the provider that has nothing to fill in and holds it to being
/// free in *every* counter.
const SEAT_BILLED: &[&str] = &["github-copilot"];

/// Every row carries the price it ought to — which for almost all of them
/// means a real one obeying the three relationships below, and for a
/// seat-billed provider means none at all — and every row is sized, with
/// no exceptions: sizing is what a session compacts against.
#[test]
fn every_row_is_priced_and_sized() {
    let snapshot = snapshot();
    assert!(!snapshot.models.is_empty());

    for model in &snapshot.models {
        assert!(!model.id.is_empty(), "{model:?}");
        assert!(!model.name.is_empty(), "{model:?}");
        assert!(model.context_window > 0, "{model:?}");
        assert!(
            model.max_output > 0 && model.max_output <= model.context_window,
            "a reply cannot exceed the window: {model:?}"
        );

        if SEAT_BILLED.contains(&model.provider_id.as_str()) {
            assert_eq!(
                (
                    model.pricing.input,
                    model.pricing.output,
                    model.pricing.cache_read,
                    model.pricing.cache_write,
                ),
                (0.0, 0.0, 0.0, None),
                "a seat has no per-token rate, so this row reports none — a \
                     partly filled one would bill somebody for tokens their \
                     subscription already covers: {model:?}"
            );
            continue;
        }

        assert!(model.pricing.input > 0.0, "{model:?}");
        assert!(
            model.pricing.output >= model.pricing.input,
            "output has never been cheaper than input: {model:?}"
        );
        assert!(
            model.pricing.cache_read < model.pricing.input,
            "a cache read is the discount: {model:?}"
        );
    }
}

#[test]
fn ids_are_unique_so_a_lookup_cannot_be_ambiguous() {
    let snapshot = snapshot();
    let mut ids: Vec<&str> = snapshot.models.iter().map(|model| model.id.as_str()).collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();

    assert_eq!(ids.len(), total, "duplicate model id in the table");
}

#[test]
fn a_known_model_resolves_and_an_unknown_one_does_not() {
    let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");

    assert_eq!(sonnet.provider_id, "anthropic");
    assert_eq!(sonnet.context_window, 1_000_000);
    assert!(model("claude-sonnet-3-nonexistent").is_none());
    // The fake provider's model is deliberately absent: nothing canned has
    // a price.
    assert!(model("canned").is_none());
}

/// The two tiers a provider now sits in, and what each owes this table.
///
/// **Selectable** is what a session may run as: the builtins in
/// `provider::PROVIDERS`, plus whatever a config's `provider` table
/// declares. **Cataloged** — [`carries`] — is the narrower set that has
/// rows here. The obligation runs one way only: a *cataloged* provider
/// must have a default that this table can size and price, because that is
/// what resolves the model when the user names none. An *uncataloged* one
/// owes this table nothing and must not pretend to: it has to have no
/// default at all, or a session would be handed a model no row can price
/// and no window can size.
///
/// The list of builtins is derived from `provider::PROVIDERS` rather than
/// written out again, for the reason it always was: two hand-maintained
/// lists in different modules is precisely how a provider gets added on
/// one side and forgotten on the other.
///
/// **What this deliberately no longer asserts** is that every builtin has
/// rows. It cannot: `fake` has none by design, a configured endpoint has
/// none by construction, and a wire may ship before its rows do. Each real
/// wire's default is pinned individually instead —
/// `each_openai_wire_defaults_to_a_model_that_wire_can_run_tools_on` here,
/// and `a_*_session_that_names_no_model_gets_one_the_catalog_can_price` in
/// `provider/{anthropic,grok,copilot}.rs` — so a forgotten row still
/// reddens something that names the provider it belongs to.
///
/// **And the obligation has an exception now, deliberately.** It was
/// written for single-vendor rosters, where "cataloged" and "one obvious
/// default" arrive together. A *gateway* breaks that pairing: each of
/// [`GATEWAYS`] is fully cataloged once a catalog is fetched — rows for
/// every vendor it fronts — and pins nothing, because no vendor and no
/// upstream rule supplies a default for one (`provider::openrouter` holds
/// the three reasons; `provider::opencode` inherits them, and would have to
/// pick between two rosters besides). They reach the uncataloged arm below
/// in *this* process only because the compiled-in snapshot carries no rows
/// for them, which is an accident of the tier rather than the reason, so
/// they are named for what they are instead of passing by coincidence.
/// `ganja-core`'s `tests/catalog_openrouter.rs` and
/// `tests/opencode_dialects.rs` are where the rows are proved present
/// against a real catalog.
/// The providers that front other people's models, which is what makes
/// "cataloged" and "has a default" come apart for them.
const GATEWAYS: [&str; 3] = [
    crate::provider::openrouter::ID,
    crate::provider::opencode::ZEN_ID,
    crate::provider::opencode::GO_ID,
];

#[test]
fn every_selectable_provider_has_a_default_this_table_can_price() {
    for provider in crate::provider::PROVIDERS {
        // The one uncataloged pin: cursor's default is `default`, the
        // server-side Auto id the wire's own listing serves — the backend
        // publishes the id, so the table does not have to. Every other
        // uncataloged provider still must not pin one.
        if provider == "cursor" {
            assert!(!carries(provider), "cursor grew rows; move it below");
            assert_eq!(
                default_model(provider),
                Some("default"),
                "cursor's pin is the wire's own Auto id, nothing else"
            );
            continue;
        }
        // The gateways: cataloged wherever a catalog was fetched, pinned
        // nowhere, and asserted as *both* rather than left to whichever
        // tier this process happens to be holding.
        if GATEWAYS.contains(&provider) {
            assert_eq!(
                default_model(provider),
                None,
                "a gateway fronting many vendors pins none of them; see \
                     `provider::{{openrouter,opencode}}` before adding one"
            );
            continue;
        }
        if !carries(provider) {
            assert!(
                default_model(provider).is_none(),
                "{provider} has no rows here, so a default would name a model \
                     this table can neither size nor price"
            );
            continue;
        }

        let id = default_model(provider)
            .unwrap_or_else(|| panic!("{provider} is cataloged but has no default model"));
        let info =
            model(id).unwrap_or_else(|| panic!("{provider}'s default {id} is not in the table"));

        assert_eq!(info.provider_id, provider, "{id} is not {provider}'s");
    }

    // The tier predicate at its boundary, on the two shapes an
    // uncataloged provider without a wire-published default takes. `fake`
    // is one this build ships and deliberately does not price;
    // `local-llama` is a config-named endpoint, which no published
    // catalog can ever know. `cursor` left this list when its pin landed
    // above: its wire publishes the id, which neither of these can claim.
    for uncataloged in [crate::provider::fake::ID, "local-llama"] {
        assert!(!carries(uncataloged), "{uncataloged} is not something this table has rows for");
        assert!(
            default_model(uncataloged).is_none(),
            "{uncataloged} has no rows, so it must have no default either"
        );
    }

    assert!(default_model("nonexistent").is_none());
    assert!(!carries("nonexistent"));
}

/// Being in the table is not enough to be a *default*: the default is what
/// a session runs when nobody chose, and every ganja session offers tools.
///
/// This vendor has **two backends with different offerings**, so it has two
/// defaults and each is held to its own backend rather than to both. That
/// is the correction the live pass forced: `gpt-5.6` was refused by the
/// ChatGPT seat (`400 "The 'gpt-5.6' model is not supported when using
/// Codex with a ChatGPT account."`) *and* by chat completions (`400
/// "Function tools with reasoning_effort are not supported for gpt-5.6 in
/// /v1/chat/completions. To use function tools, use /v1/responses…"`), and
/// answering the second by moving a key session onto the Responses API left
/// only the first — which is a fact about a seat and not about the table.
///
/// - The **key** wire's default is this table's row. It is deliberately
///   *not* held to `responses::serves`: that predicate is the subscription
///   backend's product decision, and applying it here would let somebody
///   else's seat dictate what an API key defaults to.
/// - The **subscription** wire's default is
///   `responses::SUBSCRIPTION_DEFAULT`, which must satisfy that predicate,
///   because a default the backend refuses is a seat that cannot take a
///   turn at all.
///
/// Both have to be rows this table can size and price, or the turn they
/// start has no context window and no bill.
#[test]
fn each_openai_wire_defaults_to_a_model_that_wire_can_run_tools_on() {
    let key_wire = default_model("openai").expect("openai has a pinned default");
    let subscription = crate::provider::responses::SUBSCRIPTION_DEFAULT;

    for id in [key_wire, subscription] {
        assert!(
            model(id).is_some_and(|info| info.provider_id == "openai"),
            "{id} is a default nothing can size or price"
        );
    }
    assert!(
        crate::provider::responses::serves(subscription),
        "{subscription} is what a ChatGPT seat asks for when nobody chose, \
             and its own backend refuses it"
    );
    assert!(
        model("gpt-5.6").is_some(),
        "the row the live pass refused is kept rather than deleted: its \
             sizing was never the thing that was wrong, and the wire that \
             refused it is not the wire a key rides any more"
    );
}

/// A fetched catalog names providers the way upstream does, because it is
/// upstream's file; a table nothing can look a provider up in is a table
/// that silently stops pricing that provider the first time somebody runs
/// `ganja models --refresh`.
#[test]
fn a_fetched_row_is_filed_under_the_name_the_provider_reports() {
    let fetched = parse(
        r#"{"xai":{"models":{"grok-4.3":{"name":"Grok 4.3","limit":{"context":1000000,
                "output":30000},"cost":{"input":1.25,"output":2.5}}}},
                "openai":{"models":{"gpt-5.6":{"name":"GPT-5.6","limit":{"context":1050000,
                "output":128000},"cost":{"input":5,"output":30}}}}}"#,
    )
    .expect("the payload decodes");

    let grok = fetched
        .models
        .iter()
        .find(|model| model.id == "grok-4.3")
        .expect("the row survived decoding");

    assert_eq!(
        grok.provider_id, "grok",
        "`xai` is the name on disk; every table lookup holds a `Provider::id`"
    );
    // Only the aliased one is translated. Everything else is already
    // spelled the way both projects spell it.
    assert!(
        fetched.models.iter().any(|model| model.id == "gpt-5.6" && model.provider_id == "openai")
    );
}

#[test]
fn a_turn_with_cache_traffic_prices_every_counter() {
    let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");
    let usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        reasoning_tokens: 500_000,
        cache_read_tokens: 2_000_000,
        cache_write_tokens: 1_000_000,
    };

    let Cost { input_usd, output_usd, total_usd } = cost(&usage, &sonnet);

    // 1 MTok fresh at $2 + 2 MTok cached at $0.20 + 1 MTok written at $2.50.
    assert!(close(input_usd, 2.0 + 0.4 + 2.5), "got {input_usd}");
    // Output is $10/MTok and the reasoning tokens are part of it, not extra.
    assert!(close(output_usd, 10.0), "got {output_usd}");
    assert!(close(total_usd, input_usd + output_usd));
}

/// A provider that does not bill cache writes apart still has them priced,
/// at the plain input rate, rather than silently free.
#[test]
fn a_cache_write_without_its_own_price_bills_as_input() {
    let nano = model("gpt-5.4-nano").expect("the snapshot carries nano");
    assert!(nano.pricing.cache_write.is_none());

    let usage = Usage { cache_write_tokens: 1_000_000, ..Usage::default() };

    assert!(close(cost(&usage, &nano).input_usd, nano.pricing.input));
}

#[test]
fn a_token_count_stays_readable_at_every_magnitude() {
    let cases = [
        (0, "0"),
        (7, "7"),
        (999, "999"),
        (1_000, "1.0k"),
        (12_345, "12.3k"),
        (999_949, "999.9k"),
        // The boundary rounding would otherwise print as "1000.0k".
        (999_950, "1.0M"),
        (1_000_000, "1.0M"),
        (1_050_000, "1.1M"),
    ];

    for (tokens, expected) in cases {
        assert_eq!(compact_tokens(tokens), expected, "for {tokens}");
    }
}

#[test]
fn an_empty_turn_costs_nothing() {
    let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");

    assert_eq!(cost(&Usage::default(), &sonnet), Cost::default());
}

/// Rounding to the four decimals the status bar shows must not swallow a
/// short turn: a thousand-token exchange still registers.
#[test]
fn a_short_turn_is_still_worth_a_visible_amount() {
    let opus = model("claude-opus-5").expect("the snapshot carries opus");
    let usage = Usage { input_tokens: 12_000, output_tokens: 800, ..Usage::default() };

    let total = cost(&usage, &opus).total_usd;

    assert!(close(total, 0.06 + 0.02), "got {total}");
}

/// The published payload carries a great deal this build does not read,
/// and leaves out a great deal it does — neither is a reason to refuse it.
#[test]
fn a_published_row_parses_through_fields_this_build_does_not_know() {
    let catalog = parse(&payload()).expect("the fixture is a catalog");

    let large = catalog
        .models
        .iter()
        .find(|model| model.id == "fixture-large")
        .expect("the fixture's large model is in the table");

    assert_eq!(large.provider_id, "fixture", "the provider is the outer key");
    assert_eq!(large.name, "Fixture Large");
    assert_eq!(large.context_window, 500_000);
    assert_eq!(large.max_output, 32_000);
    assert_eq!(large.input_limit, Some(400_000));
    assert_eq!(large.family.as_deref(), Some("fixture"));
    assert_eq!(large.release_date.as_deref(), Some("2026-01-31"));
    assert_eq!(large.status, ModelStatus::Beta);
    assert!(large.tool_call);
    assert!(close(large.pricing.input, 4.0));
    assert_eq!(large.pricing.cache_write, Some(5.0));
    assert_eq!(
        large.variants.keys().collect::<Vec<_>>(),
        ["max", "mini"],
        "the published variant names are the table's"
    );
    assert_eq!(
        large.variants["mini"],
        serde_json::json!({ "reasoningEffort": "low" })
            .as_object()
            .cloned()
            .expect("the fixture variant is an object"),
        "a variant's option map arrives verbatim"
    );

    let small = catalog
        .models
        .iter()
        .find(|model| model.id == "fixture-small")
        .expect("a row carrying only its limits is still a row");

    assert_eq!(small.name, "fixture-small", "an unnamed model is its id");
    assert!(close(small.pricing.input, 0.0), "an unpriced model is free");
    assert_eq!(small.pricing.cache_write, None);
    assert_eq!(small.input_limit, None);
    assert_eq!(small.status, ModelStatus::Active, "absent means active");
    assert!(small.tool_call, "absent means it takes tools");
    assert!(
        small.variants.is_empty(),
        "a row publishing no variants has none, not a parse failure"
    );
}

/// The synthesis end to end: a payload shaped exactly like the published
/// api.json — capability flags, `reasoning_options`, no declared
/// `variants` — comes out of `parse` with a roster already assembled, in
/// the splice shape of the wire that provider's rows ride.
/// The transport hint survives the way in, both halves of it.
///
/// This field exists for exactly one caller — `provider::opencode`, whose
/// gateway serves three dialects off one base URL and has nothing but this
/// to tell them apart — so "the catalog kept it" is the whole feature. A
/// row that overrides its provider keeps its own; every row that does not
/// inherits the provider's, which is upstream's `??` and not a default
/// invented here.
#[test]
fn a_rows_transport_is_its_own_or_its_providers_and_never_nothing() {
    let catalog = parse(&payload()).expect("the fixture is a catalog");
    let npm = |id: &str| {
        catalog
            .models
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("{id} is in the table"))
            .npm
            .clone()
    };

    assert_eq!(
        npm("fixture-large"),
        Some("@fixture/sdk".to_owned()),
        "a row that overrides nothing is served by its provider's transport"
    );
    assert_eq!(
        npm("fixture-small"),
        Some("@fixture/sdk".to_owned()),
        "including a row that carries almost nothing at all"
    );
    assert_eq!(
        npm("fixture-other-transport"),
        Some("@fixture/other".to_owned()),
        "and a row that names its own wins, which is the case the whole \
             field exists for"
    );

    // The snapshot is every provider whose dialect is fixed at the
    // provider, so it has nothing to say and says nothing — rather than
    // saying something that would then have to be ignored.
    assert!(
        snapshot().models.iter().all(|model| model.npm.is_none()),
        "the compiled-in tier claims no transport for anybody"
    );
}

#[test]
fn a_reasoning_row_synthesizes_its_roster_at_parse() {
    let body = r#"{
          "anthropic": {
            "models": {
              "claude-opus-4-8": {
                "reasoning": true,
                "reasoning_options": [
                  { "type": "effort", "values": ["low", "medium", "high", "xhigh", "max"] }
                ],
                "limit": { "context": 1000000, "output": 128000 }
              },
              "claude-haiku-4-5": {
                "reasoning": true,
                "reasoning_options": [{ "type": "budget_tokens", "min": 1024 }],
                "limit": { "context": 200000, "output": 64000 }
              }
            }
          },
          "openai": {
            "models": {
              "gpt-5.2": {
                "reasoning": true,
                "reasoning_options": [
                  { "type": "effort", "values": ["none", "low", "medium", "high", "xhigh"] }
                ],
                "release_date": "2025-12-11",
                "limit": { "context": 400000, "output": 128000 }
              }
            }
          }
        }"#;
    let catalog = parse(body).expect("the fixture is a catalog");
    let row = |id: &str| {
        catalog
            .models
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("{id} parses into the table"))
    };

    assert_eq!(
        serde_json::to_value(&row("claude-opus-4-8").variants["high"])
            .expect("an entry serializes"),
        serde_json::json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "effort": "high",
        }),
        "an anthropic effort splices as the Messages wire's adaptive shape"
    );
    assert_eq!(
        serde_json::to_value(&row("claude-haiku-4-5").variants["max"])
            .expect("an entry serializes"),
        serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 31999}}),
        "a budget option splices as a Messages thinking budget"
    );
    assert_eq!(
        serde_json::to_value(&row("gpt-5.2").variants["xhigh"]).expect("an entry serializes"),
        serde_json::json!({
            "reasoning": {"effort": "xhigh", "summary": "auto"},
            "include": ["reasoning.encrypted_content"],
        }),
        "an openai effort splices as the Responses wire's reasoning shape"
    );
}

/// Two providers publishing the same id must answer separately: the
/// copilot row's efforts splice as chat completions where the openai
/// row's splice as Responses, and handing a session the wrong one is a
/// body its wire cannot carry.
#[test]
fn model_for_answers_the_named_providers_row_where_ids_collide() {
    let body = r#"{
          "github-copilot": {
            "models": {
              "gpt-5.4": {
                "reasoning": true,
                "limit": { "context": 128000, "output": 64000 }
              }
            }
          },
          "openai": {
            "models": {
              "gpt-5.4": {
                "reasoning": true,
                "limit": { "context": 400000, "output": 128000 }
              }
            }
          }
        }"#;
    // Probed through `scoped` rather than installed: the process-global
    // table is what every sibling test reads, and these assertions need a
    // fixture, not the snapshot.
    let catalog = parse(body).expect("the fixture is a catalog");

    let copilot = super::scoped(&catalog, "github-copilot", "gpt-5.4")
        .expect("the copilot row is in the table");
    assert!(
        copilot.variants["high"].contains_key("reasoning_effort"),
        "the copilot row speaks chat completions"
    );
    let openai =
        super::scoped(&catalog, "openai", "gpt-5.4").expect("the openai row is in the table");
    assert!(openai.variants["high"].contains_key("reasoning"), "the openai row speaks Responses");
    assert!(
        super::scoped(&catalog, "anthropic", "gpt-5.4").is_none(),
        "a provider that does not serve the id has no row to answer with"
    );
}

/// A row that does not say what it holds cannot size a session, and a zero
/// window would have every turn compacting against nothing.
#[test]
fn a_row_that_names_no_limits_is_left_out() {
    let catalog = parse(&payload()).expect("the fixture is a catalog");

    assert!(
        !catalog.models.iter().any(|model| model.id == "fixture-unsized"),
        "an unsized row must not reach the table"
    );
}

#[test]
fn a_payload_that_holds_no_usable_row_is_not_a_catalog() {
    for body in [
        "{}",
        r#"{"fixture": {"models": {}}}"#,
        r#"{"fixture": {"models": {"m": {"limit": {"context": 0, "output": 0}}}}}"#,
        "not json at all",
    ] {
        assert!(parse(body).is_err(), "{body} should not be a catalog");
    }
}

/// One drifted row is not a reason to throw away every other row that came
/// with it.
#[test]
fn a_row_whose_shape_drifted_is_skipped_rather_than_fatal() {
    let body = r#"{
          "fixture": {
            "models": {
              "drifted": { "limit": "as much as you like" },
              "sound": { "limit": { "context": 1000, "output": 100 } }
            }
          },
          "shapeless": { "models": [] }
        }"#;

    let catalog = parse(body).expect("the sound row still makes a catalog");

    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].id, "sound");
}

#[test]
fn a_source_other_than_the_default_gets_its_own_cache_file() {
    assert_eq!(cache_name(DEFAULT_SOURCE), "models.json");

    let mirror = cache_name("https://models.example.test");
    let other = cache_name("https://models.example.test/v2");

    assert!(mirror.starts_with("models-") && mirror.ends_with(".json"), "{mirror}");
    assert_ne!(mirror, other, "two mirrors cannot share one cache file");
    assert_eq!(
        mirror,
        cache_name("https://models.example.test"),
        "the same URL names the same file every run"
    );
}

#[test]
fn a_cached_body_is_written_verbatim_and_read_back() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("nested").join("models.json");
    let body = payload();

    write_cache(&path, &body).expect("the cache is writable");

    assert_eq!(
        fs::read_to_string(&path).expect("the cache is readable"),
        body,
        "what is cached is the bytes that arrived"
    );

    let source = Source {
        url: DEFAULT_SOURCE.to_owned(),
        cache: path.clone(),
        read: path,
        overridden: false,
    };
    let catalog = read_cached(&source).expect("the cache holds a catalog");

    // Against the parse of the same bytes rather than a number: what this
    // test is about is the round trip, and a literal count would have to be
    // edited every time the shared fixture grows a row.
    assert_eq!(
        catalog.models.len(),
        parse(&payload()).expect("the fixture is a catalog").models.len(),
        "reading the cache back yields the table the payload parses to"
    );
}

/// A cache that cannot be read is the one thing a stale-tolerant reader
/// must not keep serving, and the one thing it can safely remove.
#[test]
fn an_unreadable_cache_is_deleted_and_read_as_absent() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("models.json");
    fs::write(&path, "{\"truncated\": ").expect("the fixture is writable");

    let source = Source {
        url: DEFAULT_SOURCE.to_owned(),
        cache: path.clone(),
        read: path.clone(),
        overridden: false,
    };

    assert!(read_cached(&source).is_none());
    assert!(!path.exists(), "a cache this build wrote is its to remove");
}

#[test]
fn a_cache_the_environment_named_is_read_as_absent_but_never_deleted() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let named = directory.path().join("somebody-elses.json");
    let cache = directory.path().join("models.json");
    fs::write(&named, "{\"truncated\": ").expect("the fixture is writable");
    fs::write(&cache, payload()).expect("the fixture is writable");

    let source = Source {
        url: DEFAULT_SOURCE.to_owned(),
        cache: cache.clone(),
        read: named.clone(),
        overridden: true,
    };

    assert!(read_cached(&source).is_none());
    assert!(named.exists(), "an overridden path is not this build's file");
    assert!(cache.exists(), "and neither is the cache it stood in for");
}

/// The freshness gate is what keeps a refresh from re-fetching a catalog
/// somebody else just wrote, and what stops it from skipping one that has
/// aged out.
#[test]
fn a_cache_is_fresh_until_the_debounce_has_passed() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("models.json");

    assert!(!fresh(&path), "a cache that does not exist is not fresh");

    fs::write(&path, payload()).expect("the fixture is writable");
    assert!(fresh(&path), "a cache written just now is fresh");

    let file = fs::File::options().write(true).open(&path).expect("the fixture is openable");
    file.set_modified(std::time::SystemTime::now() - super::DEBOUNCE - Duration::from_secs(1))
        .expect("the fixture's timestamp is settable");

    assert!(!fresh(&path), "a cache older than the debounce is not");
}

/// A rename onto a non-empty directory cannot succeed, which is the
/// cheapest way to fail the last step of a write and see what it leaves
/// behind.
#[test]
fn a_cache_write_that_fails_leaves_no_temporary_behind() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("models.json");
    fs::create_dir_all(path.join("occupied")).expect("the obstruction is creatable");

    write_cache(&path, &payload()).expect_err("a rename onto a directory cannot succeed");

    let strays: Vec<_> = fs::read_dir(directory.path())
        .expect("the directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();

    assert!(strays.is_empty(), "a failed write left {strays:?} behind");
}

/// The window's own edges, which sixty-four live draws can only suggest.
#[test]
fn a_backoff_walks_half_to_half_again_of_the_attempts_base() {
    for attempt in 0..3 {
        let base = super::RETRY_BASE * 2_u32.pow(attempt);

        assert_eq!(scattered(attempt, 0), base.mul_f64(0.5), "the smallest draw is half the base");
        let widest = scattered(attempt, 999_999);
        assert!(
            widest > base.mul_f64(1.499) && widest < base.mul_f64(1.5),
            "the largest draw stops just short of half again, got {widest:?}"
        );
    }
}

#[test]
fn the_backoff_grows_and_never_lands_on_the_same_edge() {
    for attempt in 0..3 {
        let waited = backoff(attempt);
        let base = super::RETRY_BASE * 2_u32.pow(attempt);

        assert!(
            waited >= base.mul_f64(0.5) && waited <= base.mul_f64(1.5),
            "attempt {attempt} waited {waited:?}, off the jitter window around {base:?}"
        );
    }

    assert!(
        scattered(2, 0) > scattered(0, 999_999),
        "the last wait is longer than the first can ever be"
    );
}

/// Nothing in the table is handed out as a borrow of the tier it came
/// from: a refresh replaces the whole table, and a caller still holding a
/// row from before it must keep reading the row it asked for. A `&'static`
/// row could not have survived the tier being dropped, and a copied one
/// would not have been the same row.
#[test]
fn a_row_outlives_the_table_it_came_from() {
    let catalog = parse(&payload()).expect("the fixture is a catalog");
    let held = Arc::clone(&catalog.models[0]);
    let rows = Arc::strong_count(&held);

    drop(catalog);

    assert_eq!(Arc::strong_count(&held), rows - 1, "the table let go of it");
    assert_eq!(held.id, "fixture-large");
    assert_eq!(held.context_window, 500_000);
}

#[test]
fn a_status_this_build_does_not_know_reads_as_active() {
    assert_eq!(super::status(None), ModelStatus::Active);
    assert_eq!(super::status(Some("alpha")), ModelStatus::Alpha);
    assert_eq!(super::status(Some("beta")), ModelStatus::Beta);
    assert_eq!(super::status(Some("deprecated")), ModelStatus::Deprecated);
    assert_eq!(super::status(Some("retired")), ModelStatus::Active);
}

/// The snapshot is what makes the table unconditional; it must be a table
/// on its own, with no filesystem and no network anywhere near it.
#[test]
fn the_snapshot_stands_alone() {
    let snapshot = snapshot();

    assert_eq!(snapshot.models.len(), super::SNAPSHOT.len());
    assert!(
        snapshot.models.iter().all(|model| model.status == ModelStatus::Active && model.tool_call),
        "every compiled-in row is a current tool-using model"
    );
    assert_eq!(
        Pricing { input: 2.0, output: 10.0, cache_read: 0.2, cache_write: Some(2.5) },
        snapshot.models[0].pricing
    );
}
