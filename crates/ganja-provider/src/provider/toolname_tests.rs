use super::*;

/// The exact name that killed a live turn, colons and all.
const FIELD: &str = "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: "for the roster".to_owned(),
        schema: serde_json::json!({"type": "object"}),
    }
}

#[test]
fn a_name_the_vendors_already_accept_is_advertised_byte_identically() {
    for name in ["read", "todowrite", "mcp__docs__search", "plan-exit_1"] {
        assert!(
            matches!(alias(name, OPENAI_CAP), Cow::Borrowed(kept) if kept == name),
            "{name} should not have been rewritten"
        );
    }
}

#[test]
fn the_field_name_that_was_refused_becomes_one_the_pattern_accepts() {
    assert_eq!(FIELD.chars().count(), 69, "the reported length");

    let aliased = alias(FIELD, OPENAI_CAP);

    assert_eq!(aliased.len(), OPENAI_CAP);
    assert!(aliased.bytes().all(conforming), "{aliased} is outside [A-Za-z0-9_-]");
    // Pinned rather than merely described: determinism is what lets an
    // encoder and a decoder recompute the same string, and what lets a
    // transcript replayed on a later turn name what that turn advertises.
    // A change to this literal is a change to both, never one of them.
    assert_eq!(aliased, "mcp__plugin_mcp-gemini-search_mcp-gemini-search__deep_r_6bb398bf");
}

/// Anthropic's own cap is wide enough that the same name only loses its
/// colons — a truncation there would be a rewrite nobody's API asked for.
#[test]
fn the_same_name_only_loses_its_colons_under_the_wider_cap() {
    assert_eq!(
        alias(FIELD, ANTHROPIC_CAP),
        "mcp__plugin_mcp-gemini-search_mcp-gemini-search__deep_research_result"
    );
}

#[test]
fn aliasing_the_same_name_twice_gives_the_same_string() {
    assert_eq!(alias(FIELD, OPENAI_CAP), alias(FIELD, OPENAI_CAP));
}

/// The whole reason the truncation carries a digest: a shared prefix long
/// enough to survive the cut must not become a shared alias.
#[test]
fn two_long_names_sharing_a_prefix_never_alias_equal() {
    let stem = "mcp__plugin:some-very-long-marketplace-name:some-server__";
    let (first, second) = (format!("{stem}first_tool_here"), format!("{stem}second_tool"));
    let one = alias(&first, OPENAI_CAP);
    let two = alias(&second, OPENAI_CAP);

    assert_eq!(one.len(), OPENAI_CAP);
    assert_eq!(two.len(), OPENAI_CAP);
    assert_ne!(one, two);
    assert_eq!(one[..OPENAI_CAP - HASH], two[..OPENAI_CAP - HASH]);
}

/// A name outside ASCII scrubs one underscore per *character*, so the
/// result stays cuttable on any byte.
#[test]
fn a_name_outside_ascii_scrubs_to_one_underscore_per_character() {
    assert_eq!(alias("読み込み_tool", OPENAI_CAP), "_____tool");
}

#[test]
fn a_conforming_roster_builds_no_map_at_all() {
    let aliases = Aliases::of(&[definition("read"), definition("write")], OPENAI_CAP);

    assert_eq!(aliases.original("read".to_owned()), "read");
    assert!(aliases.0.is_empty());
}

#[test]
fn an_aliased_call_comes_back_out_under_the_name_the_engine_registered() {
    let aliases = Aliases::of(&[definition(FIELD), definition("read")], OPENAI_CAP);
    let aliased = alias(FIELD, OPENAI_CAP).into_owned();

    assert_eq!(aliases.original(aliased), FIELD);
}

/// A model that invented a tool must be reported under the name it said,
/// because that is what the engine's unknown-tool error text has to name.
#[test]
fn a_name_the_map_never_saw_passes_through_unchanged() {
    let aliases = Aliases::of(&[definition(FIELD)], OPENAI_CAP);

    assert_eq!(aliases.original("invented_tool".to_owned()), "invented_tool");
}
