use std::collections::BTreeMap;

use serde_json::json;

use super::{
    budget_efforts, claude_version, gpt5_version, is_gpt5_family, is_gpt5_pro,
    openai_reasoning_efforts, roster,
};
use crate::catalog::{ModelInfo, ModelStatus, Pricing, ReasoningOption};

/// A reasoning row under `provider_id`, sized like a real one, with no
/// declared variants and no capability options until a test adds them.
fn model(provider_id: &str, id: &str, max_output: u64) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        provider_id: provider_id.to_owned(),
        name: id.to_owned(),
        context_window: 200_000,
        max_output,
        input_limit: None,
        pricing: Pricing { input: 1.0, output: 2.0, cache_read: 0.1, cache_write: None },
        family: None,
        release_date: None,
        tool_call: true,
        status: ModelStatus::Active,
        reasoning: true,
        reasoning_options: None,
        npm: None,
        variants: BTreeMap::new(),
    }
}

/// The published-option shapes the real api.json carries.
fn efforts(values: &[Option<&str>]) -> ReasoningOption {
    ReasoningOption::Effort {
        values: values.iter().map(|value| value.map(str::to_owned)).collect(),
    }
}

#[test]
fn an_anthropic_budget_model_gets_thinking_budgets_in_the_messages_shape() {
    // claude-haiku-4-5 as published: a budget floor and no ceiling.
    let mut haiku = model("anthropic", "claude-haiku-4-5", 64_000);
    haiku.reasoning_options =
        Some(vec![ReasoningOption::BudgetTokens { min: Some(1024.0), max: None }]);

    let roster = roster(&haiku);
    assert_eq!(
        serde_json::to_value(&roster).expect("a roster serializes"),
        json!({
            "high": {"thinking": {"type": "enabled", "budget_tokens": 16_000}},
            "max": {"thinking": {"type": "enabled", "budget_tokens": 31_999}},
        }),
        "half of the clamped ceiling floored, and the ceiling itself"
    );
}

#[test]
fn an_anthropic_adaptive_model_synthesizes_maps_from_its_published_values() {
    let mut opus = model("anthropic", "claude-opus-4-8", 128_000);
    opus.reasoning_options = Some(vec![efforts(&[
        Some("low"),
        Some("medium"),
        Some("high"),
        Some("xhigh"),
        Some("max"),
    ])]);

    let synthesized = roster(&opus);
    assert_eq!(synthesized.len(), 5);
    assert_eq!(
        serde_json::to_value(&synthesized["xhigh"]).expect("an entry serializes"),
        json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "xhigh"}),
        "4.8 is modern adaptive, which forces the summarized display"
    );

    // The 4.6 pair speaks adaptive too, but defaults its display.
    let mut sonnet = model("anthropic", "claude-sonnet-4-6", 64_000);
    sonnet.reasoning_options = Some(vec![efforts(&[Some("low"), Some("max")])]);
    assert_eq!(
        serde_json::to_value(&roster(&sonnet)["max"]).expect("an entry serializes"),
        json!({"thinking": {"type": "adaptive"}, "effort": "max"}),
    );
}

#[test]
fn the_anthropic_table_answers_when_the_row_publishes_no_options() {
    // No `reasoning_options` at all: the hardcoded table speaks. 4.5-era
    // haiku is neither modern adaptive nor in the 4.6 pair nor opus-4.5,
    // so it takes the plain budget branch.
    let haiku = model("anthropic", "claude-haiku-4-5", 64_000);
    assert_eq!(
        serde_json::to_value(roster(&haiku)).expect("a roster serializes"),
        json!({
            "high": {"thinking": {"type": "enabled", "budget_tokens": 16_000}},
            "max": {"thinking": {"type": "enabled", "budget_tokens": 31_999}},
        }),
    );

    // A versionless claude id reads as newest: the full adaptive five.
    let unversioned = model("anthropic", "claude-vnext", 64_000);
    let synthesized = roster(&unversioned);
    let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
    assert_eq!(names, ["high", "low", "max", "medium", "xhigh"]);
}

#[test]
fn an_openai_model_new_enough_for_xhigh_carries_it_and_an_old_one_does_not() {
    // gpt-5.2 is versioned past 1: none through xhigh, no date needed.
    let gated = model("openai", "gpt-5.2", 128_000);
    let synthesized = roster(&gated);
    let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
    assert_eq!(names, ["high", "low", "medium", "none", "xhigh"]);

    // gpt-5.1 swapped minimal for none and predates xhigh.
    let earlier = model("openai", "gpt-5.1", 128_000);
    assert!(!roster(&earlier).contains_key("xhigh"));
    assert!(roster(&earlier).contains_key("none"));

    // o3 is neither gpt-5 family nor released past either gate.
    let mut o3 = model("openai", "o3", 100_000);
    o3.release_date = Some("2025-04-16".to_owned());
    let synthesized = roster(&o3);
    let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
    assert_eq!(names, ["high", "low", "medium"]);

    // Every map is the Responses body's own spelling.
    assert_eq!(
        serde_json::to_value(&synthesized["high"]).expect("an entry serializes"),
        json!({
            "reasoning": {"effort": "high", "summary": "auto"},
            "include": ["reasoning.encrypted_content"],
        }),
    );
}

/// The gateway's own four levels, and the map each of them splices — which
/// is the whole of what its reference documents about reasoning and
/// deliberately none of what the sibling Responses map carries.
#[test]
fn an_openrouter_row_is_offered_the_four_efforts_its_reference_publishes() {
    let routed = model("openrouter", "openai/o4-mini", 100_000);
    let synthesized = roster(&routed);
    let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["high", "low", "medium", "minimal"],
        "the reference's four, in the schema's sorted order"
    );
    assert_eq!(
        serde_json::to_value(&synthesized["high"]).expect("an entry serializes"),
        json!({"reasoning": {"effort": "high"}}),
        "no `summary` and no `include`: both are the other vendor's fields, \
             and the openrouter ledger drops them"
    );

    // The pre-mortem's non-reasoning row: the table's own gate answers it,
    // so a chat-tuned gateway row is offered no effort to 400 over.
    let mut plain = model("openrouter", "openai/gpt-5.2-chat", 100_000);
    plain.reasoning = false;
    assert!(roster(&plain).is_empty());
}

/// Catalog-first, the file's standing rule, on the one provider whose table
/// rows are authored here: a row that publishes its own effort values is
/// answered by them and never by the four.
#[test]
fn a_published_openrouter_row_outranks_the_authored_table() {
    let mut published = model("openrouter", "anthropic/claude-sonnet-5", 64_000);
    published.reasoning_options = Some(vec![efforts(&[Some("low"), Some("max")])]);

    let synthesized = roster(&published);
    let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["low", "max"],
        "the published values, including one the authored table never lists"
    );
    assert_eq!(
        serde_json::to_value(&synthesized["max"]).expect("an entry serializes"),
        json!({"reasoning": {"effort": "max"}}),
        "still this gateway's own body shape, whoever named the effort"
    );

    // A budget-only row encodes nothing on this wire — there is no
    // documented budget field — so it falls through to the table, exactly
    // as a chat-wire row does.
    let mut budgeted = model("openrouter", "anthropic/claude-sonnet-5", 64_000);
    budgeted.reasoning_options =
        Some(vec![ReasoningOption::BudgetTokens { min: Some(1024.0), max: Some(32_000.0) }]);
    let fallen_through = roster(&budgeted);
    assert_eq!(
        fallen_through.keys().map(String::as_str).collect::<Vec<_>>(),
        ["high", "low", "medium", "minimal"],
    );
}

#[test]
fn grok_3_mini_gets_exactly_low_and_high() {
    let mini = model("grok", "grok-3-mini", 30_000);
    assert_eq!(
        serde_json::to_value(roster(&mini)).expect("a roster serializes"),
        json!({
            "low": {"reasoning_effort": "low"},
            "high": {"reasoning_effort": "high"},
        }),
    );

    // Every other reasoning grok takes the widely supported three.
    let grown = model("grok", "grok-4.5", 500_000);
    let synthesized = roster(&grown);
    let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
    assert_eq!(names, ["high", "low", "medium"]);
}

#[test]
fn copilot_claude_rides_reasoning_effort_and_gemini_stays_empty() {
    // A budget-only row encodes nothing on the chat wire, which is the
    // fall-through to the table — and the table gives Claude the three.
    let mut claude = model("github-copilot", "claude-opus-4.5", 32_000);
    claude.reasoning_options =
        Some(vec![ReasoningOption::BudgetTokens { min: Some(1024.0), max: Some(32_000.0) }]);
    assert_eq!(
        serde_json::to_value(roster(&claude)).expect("a roster serializes"),
        json!({
            "low": {"reasoning_effort": "low"},
            "medium": {"reasoning_effort": "medium"},
            "high": {"reasoning_effort": "high"},
        }),
    );

    // Copilot's Gemini publishes values, and every one is skipped: the
    // endpoint has no dial for them.
    let mut gemini = model("github-copilot", "gemini-3.5-flash", 64_000);
    gemini.reasoning_options = Some(vec![efforts(&[Some("low"), Some("high")])]);
    assert!(roster(&gemini).is_empty());

    // The GPT table rows take the xhigh gates: 5.2 by id, gpt-5 by date.
    let unversioned_old = model("github-copilot", "gpt-5", 128_000);
    assert!(!roster(&unversioned_old).contains_key("xhigh"));
    let mut unversioned_new = model("github-copilot", "gpt-5", 128_000);
    unversioned_new.release_date = Some("2025-12-04".to_owned());
    assert!(roster(&unversioned_new).contains_key("xhigh"));
    let versioned = model("github-copilot", "gpt-5.2", 128_000);
    assert!(roster(&versioned).contains_key("xhigh"));
}

#[test]
fn a_model_that_does_not_reason_synthesizes_nothing() {
    let mut plain = model("anthropic", "claude-opus-4-8", 128_000);
    plain.reasoning = false;

    assert!(roster(&plain).is_empty());
}

#[test]
fn empty_reasoning_options_mean_no_efforts_even_where_the_table_would_speak() {
    // The published empty list is "explicitly none", which the table may
    // not override — grok-4.20-0309-reasoning ships exactly this shape.
    let mut refused = model("anthropic", "claude-opus-4-8", 128_000);
    refused.reasoning_options = Some(vec![]);

    assert!(roster(&refused).is_empty());
}

#[test]
fn a_toggle_alone_falls_through_to_the_table() {
    // A toggle encodes only on transports outside this port, so a
    // toggle-only row is answered by the table, upstream's own
    // fall-through.
    let mut toggled = model("anthropic", "claude-haiku-4-5", 64_000);
    toggled.reasoning_options = Some(vec![ReasoningOption::Toggle]);

    assert_eq!(roster(&toggled), roster(&model("anthropic", "claude-haiku-4-5", 64_000)));
}

#[test]
fn budget_boundaries_clamp_to_the_output_limit_and_vanish_at_zero() {
    // The model's own output cap outranks a larger published maximum.
    let capped = model("anthropic", "claude-haiku-4-5", 8_000);
    let efforts = budget_efforts(&capped, None, Some(32_000.0));
    assert_eq!(
        efforts["high"]["thinking"]["budget_tokens"],
        serde_json::Value::from(4_000),
        "half of the capped ceiling: floor((7999 + 1) / 2)"
    );
    assert_eq!(efforts["max"]["thinking"]["budget_tokens"], serde_json::Value::from(7_999));

    // A published floor above the halfway point raises `high`…
    let raised = budget_efforts(&capped, Some(6_000.0), Some(32_000.0));
    assert_eq!(raised["high"]["thinking"]["budget_tokens"], serde_json::Value::from(6_000));
    // …but never past the ceiling.
    let pinned = budget_efforts(&capped, Some(9_000.0), Some(32_000.0));
    assert_eq!(pinned["high"]["thinking"]["budget_tokens"], serde_json::Value::from(7_999));

    // An output limit of one token leaves no room at all.
    let empty = budget_efforts(&model("anthropic", "claude-haiku-4-5", 1), None, None);
    assert!(empty.is_empty(), "maximum <= 0 yields no budget efforts");
}

#[test]
fn a_declared_variant_overrides_the_synthesized_entry_and_a_new_name_joins() {
    let mut opus = model("anthropic", "claude-opus-4-8", 128_000);
    opus.reasoning_options = Some(vec![efforts(&[Some("low"), Some("high")])]);
    opus.variants = serde_json::from_value(json!({
        "high": {"effort": "overridden"},
        "custom": {"thinking": {"type": "disabled"}},
    }))
    .expect("declared variants parse");

    let roster = roster(&opus);
    assert_eq!(
        serde_json::to_value(&roster["high"]).expect("an entry serializes"),
        json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "effort": "overridden",
        }),
        "the declared key wins while the synthesized siblings survive: mergeDeep"
    );
    assert_eq!(
        serde_json::to_value(&roster["custom"]).expect("an entry serializes"),
        json!({"thinking": {"type": "disabled"}}),
        "a name the synthesis never produced joins whole"
    );
    assert!(roster.contains_key("low"), "untouched entries stand");
}

#[test]
fn a_provider_outside_the_exclusion_rule_keeps_only_what_it_declares() {
    let mut foreign = model("mistral", "mistral-large", 64_000);
    foreign.reasoning_options = Some(vec![efforts(&[Some("high")])]);
    foreign.variants = serde_json::from_value(json!({"declared": {"reasoning_effort": "high"}}))
        .expect("declared variants parse");

    let roster = roster(&foreign);
    assert_eq!(
        roster.keys().collect::<Vec<_>>(),
        ["declared"],
        "no wire, no synthesis — the declaration alone"
    );
}

#[test]
fn the_id_matchers_read_versions_the_way_upstreams_regexes_do() {
    // The claude version pattern, including the date guard.
    assert_eq!(claude_version("claude-opus-4-8"), Some((4, 8)));
    assert_eq!(claude_version("claude-sonnet-4-6"), Some((4, 6)));
    assert_eq!(claude_version("claude-3-5-sonnet"), Some((3, 5)));
    assert_eq!(
        claude_version("claude-opus-4-20250514"),
        Some((4, 0)),
        "an eight-digit date is not a minor version"
    );
    assert_eq!(claude_version("claude-vnext"), None);
    assert_eq!(claude_version("claude-opus-٤-٦"), None);

    // The gpt-5 anchors: start or slash, never mid-word.
    assert!(is_gpt5_family("gpt-5.4-mini"));
    assert!(is_gpt5_family("openai/gpt-5.4-codex"));
    assert!(!is_gpt5_family("gpt-50"));
    assert!(!is_gpt5_family("agpt-5"));

    // `Number(...) || undefined`: a matched zero is no version.
    assert_eq!(gpt5_version("gpt-5.4-mini"), Some(4));
    assert_eq!(gpt5_version("gpt-5.0"), None);
    assert_eq!(gpt5_version("gpt-5-nano"), None);

    assert!(is_gpt5_pro("gpt-5-pro"));
    assert!(is_gpt5_pro("gpt-5pro"));
    assert!(!is_gpt5_pro("gpt-5-provider"));

    // The pro and codex tiers, straight off the model pages.
    assert_eq!(openai_reasoning_efforts("gpt-5-pro", ""), ["high"]);
    assert_eq!(openai_reasoning_efforts("gpt-5.2-pro", ""), ["medium", "high", "xhigh"]);
    assert_eq!(
        openai_reasoning_efforts("gpt-5.3-codex", ""),
        ["none", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(
        openai_reasoning_efforts("gpt-5.1-codex-max", ""),
        ["low", "medium", "high", "xhigh"]
    );
    assert_eq!(
        openai_reasoning_efforts("gpt-5-chat", ""),
        [] as [&str; 0],
        "the versionless chat model answers empty rather than falling through"
    );
    assert_eq!(openai_reasoning_efforts("o3-deep-research", ""), ["medium"]);
    assert_eq!(
        openai_reasoning_efforts("gpt-5", "2025-08-07"),
        ["minimal", "low", "medium", "high"]
    );
}
