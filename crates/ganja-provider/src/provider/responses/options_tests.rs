use std::collections::BTreeSet;

use super::*;

/// The seat's key list is the narrow one, and the only name it holds that
/// the platform's does not is the one the platform was measured to refuse:
/// `access_programs` (probe 2026-09-17, organization-gated).
///
/// Written out separately rather than concatenated (a `const` cannot
/// concatenate slices), which is exactly why this has to be a test: a key
/// added to the seat's list and forgotten in the platform's would make a
/// document that loads under `chatgpt` refuse under `openai`, and only a
/// measurement is allowed to do that.
#[test]
fn the_seat_takes_no_key_the_platform_refuses_but_the_one_measured() {
    let seat: BTreeSet<&str> = SEAT_ACCEPTED.iter().copied().collect();
    let platform: BTreeSet<&str> = PLATFORM_ACCEPTED.iter().copied().collect();

    assert_eq!(
        seat.difference(&platform).copied().collect::<Vec<_>>(),
        ["access_programs"],
        "the seat's list holds a key the platform's is missing"
    );
    assert!(
        platform.contains("context_management") && !seat.contains("context_management"),
        "context_management was measured to do nothing on the seat (probe 2026-09-17)"
    );
}

/// The same claim for the three value lists: the platform takes every value
/// the seat does, except `ultrafast`, which it answered with a 500 (probe
/// 2026-09-17).
#[test]
fn the_seat_takes_no_value_the_platform_refuses_but_ultrafast() {
    for (name, seat, platform, measured) in [
        ("service_tier", SEAT_TIERS, PLATFORM_TIERS, &["ultrafast"][..]),
        ("server_tools", SEAT_SERVER_TOOLS, PLATFORM_SERVER_TOOLS, &[][..]),
        ("include", SEAT_INCLUDE, PLATFORM_INCLUDE, &[][..]),
    ] {
        let seat: BTreeSet<&str> = seat.iter().copied().collect();
        let platform: BTreeSet<&str> = platform.iter().copied().collect();

        assert_eq!(
            seat.difference(&platform).copied().collect::<Vec<_>>(),
            measured,
            "the platform's {name} list is missing a value the seat takes"
        );
    }
    assert!(!PLATFORM_TIERS.contains(&"scale"), "scale was refused on the platform too");
}

/// No list holds a name twice, and none is empty — a duplicate would make a
/// refusal sentence read absurdly, and an empty list would silently refuse
/// everything.
#[test]
fn no_list_repeats_a_name_or_is_empty() {
    for (name, list) in [
        ("SEAT_ACCEPTED", SEAT_ACCEPTED),
        ("PLATFORM_ACCEPTED", PLATFORM_ACCEPTED),
        ("SEAT_TIERS", SEAT_TIERS),
        ("PLATFORM_TIERS", PLATFORM_TIERS),
        ("SEAT_SERVER_TOOLS", SEAT_SERVER_TOOLS),
        ("PLATFORM_SERVER_TOOLS", PLATFORM_SERVER_TOOLS),
        ("SEAT_INCLUDE", SEAT_INCLUDE),
        ("PLATFORM_INCLUDE", PLATFORM_INCLUDE),
        ("CLIENT_SIDE_SERVER_TOOLS", CLIENT_SIDE_SERVER_TOOLS),
        ("CUSTOM_TOOLS", CUSTOM_TOOLS),
    ] {
        assert!(!list.is_empty(), "{name} is empty");

        let unique: BTreeSet<&str> = list.iter().copied().collect();
        assert_eq!(unique.len(), list.len(), "{name} names something twice: {list:?}");
    }
}

/// A hosted type this build refuses on both ids is not quietly also on one of
/// the accepted lists — the two refusals say different things, and a type on
/// both lists would make which one fires depend on the order of the checks.
#[test]
fn a_client_side_tool_type_is_on_no_accepted_list() {
    for kind in CLIENT_SIDE_SERVER_TOOLS {
        assert!(!SEAT_SERVER_TOOLS.contains(kind), "{kind} is refused and accepted on the seat");
        assert!(
            !PLATFORM_SERVER_TOOLS.contains(kind),
            "{kind} is refused and accepted on the platform"
        );
    }
}

/// **The predicate, not the list.** [`CUSTOM_TOOLS`] is derived here from the
/// registry this build actually ships rather than pinned to a hand-written
/// set, so a builtin that grows a second required argument leaves the list on
/// its own, and a new builtin with one required string joins it.
///
/// Asserted in both directions: a name here that the predicate does not admit
/// would be advertised as a custom tool the model cannot fill in, and a name
/// the predicate admits and this list omits is a tool a config cannot reach.
#[test]
fn the_custom_tool_list_is_exactly_the_builtins_with_one_required_string() {
    let registry = crate::tool::Registry::with_builtins();
    let derived: BTreeSet<String> = registry
        .definitions()
        .into_iter()
        .filter(|definition| super::super::single_string_argument(&definition.schema).is_some())
        .map(|definition| definition.name)
        .collect();
    let listed: BTreeSet<String> = CUSTOM_TOOLS.iter().map(|name| (*name).to_owned()).collect();

    assert_eq!(
        listed, derived,
        "CUSTOM_TOOLS and the builtins with exactly one required string argument have drifted"
    );
}

/// The one model measured to have a fast tier of its own answers it, and
/// everything else answers the default — including a model nobody has heard
/// of, which is the case that matters: the table is a set of exceptions, not
/// a roster.
#[test]
fn only_a_model_the_table_names_gets_a_tier_of_its_own() {
    assert_eq!(fast_tier("gpt-5.6-sol"), "ultrafast");
    assert_eq!(fast_tier("gpt-5.5"), DEFAULT_FAST_TIER);
    assert_eq!(fast_tier("gpt-6-astra"), DEFAULT_FAST_TIER);
    assert_eq!(fast_tier("a-model-that-does-not-exist"), DEFAULT_FAST_TIER);
}

/// Every tier the table names is one the seat was measured to take. A row
/// naming a value the backend refuses would turn one `/fast` keystroke into a
/// turn that cannot start.
#[test]
fn every_fast_tier_is_one_the_seat_accepts() {
    for (model, tier) in FAST_TIERS {
        assert!(SEAT_TIERS.contains(tier), "{model}'s fast tier {tier} is not one the seat takes");
    }
    assert!(SEAT_TIERS.contains(&DEFAULT_FAST_TIER));
}

/// The two ids this module describes are the two ids, and nothing else is.
#[test]
fn only_the_two_responses_ids_read_options() {
    assert!(speaks_options(CHATGPT_ID));
    assert!(speaks_options(ID));
    assert!(!speaks_options("anthropic"));
    assert!(!speaks_options("openrouter"));

    assert_eq!(accepted(CHATGPT_ID), Some(SEAT_ACCEPTED));
    assert_eq!(accepted(ID), Some(PLATFORM_ACCEPTED));
    assert_eq!(accepted("anthropic"), None);

    assert_eq!(tiers(CHATGPT_ID), Some(SEAT_TIERS));
    assert_eq!(tiers(ID), Some(PLATFORM_TIERS));
    assert_eq!(tiers("anthropic"), None);

    assert_eq!(server_tools(CHATGPT_ID), Some(SEAT_SERVER_TOOLS));
    assert_eq!(server_tools(ID), Some(PLATFORM_SERVER_TOOLS));
    assert_eq!(server_tools("anthropic"), None);

    assert_eq!(include(CHATGPT_ID), Some(SEAT_INCLUDE));
    assert_eq!(include(ID), Some(PLATFORM_INCLUDE));
    assert_eq!(include("anthropic"), None);
}
