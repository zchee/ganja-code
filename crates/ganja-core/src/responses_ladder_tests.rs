use std::collections::BTreeSet;

use serde_json::json;

use super::{Seed, Source, fast_tier, resolve};
use crate::config::ResponsesOptions;
use crate::protocol::FastChoice;
use crate::provider::responses::{self, options};

/// Every key the decode struct has, set — both ids' vocabulary together.
///
/// Decoded straight into the struct rather than through the loader, whose
/// per-id gate is not what these tests are about.
const EVERY_KEY: &str = r#"
service_tier = "flex"
reasoning = { context = "all_turns", summary = "concise", mode = "pro" }
text = { verbosity = "low" }
parallel_tool_calls = false
stream_options = { include_obfuscation = true }
tool_choice = "required"
custom_tools = ["bash"]
server_tools = [{ type = "web_search", search_context_size = "low" }]
include = ["web_search_call.action.sources"]
context_management = [{ type = "compaction", compact_threshold = 1000 }]
client_metadata = { origin = "ganja" }
access_programs = { program = "research" }
max_output_tokens = 100
max_tool_calls = 3
prompt_cache_key = "k"
prompt_cache_retention = "24h"
prompt_cache_options = { mode = "explicit", ttl = "1h" }
temperature = 0.2
top_p = 0.9
top_logprobs = 2
truncation = "auto"
safety_identifier = "s"
user = "u"
metadata = { team = "core" }
moderation = { model = "omni", policy = "strict" }
"#;

/// The keys the wire's typed request body writes itself, which a layer must
/// never supply (the list fix-w2's strip in `gated` holds).
const OWN_KEYS: &[&str] =
    &["tools", "include", "instructions", "model", "stream", "store", "input"];

fn table(text: &str) -> ResponsesOptions {
    toml::from_str(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn seed(provider: &str, table: Option<ResponsesOptions>, fast: Option<FastChoice>) -> Seed {
    Seed { table, fast, provider: provider.to_owned() }
}

/// The tier a seed resolves for `model`, with its rung.
fn tier(seed: &Seed, model: &str) -> (Option<String>, Option<Source>) {
    let (resolved, source) = resolve(seed, model);
    (resolved.service_tier, source)
}

const SOL: &str = "gpt-5.6-sol";
const FIVE: &str = "gpt-5.5";

/// **AC-23**, at the resolver: the charter's own example, rung by rung, on
/// both Responses ids.
#[test]
fn the_tier_ladder_resolves_per_model_per_choice_and_per_provider() {
    let configured = table(
        "service_tier = \"priority\"\n[model.\"gpt-5.6-sol\"]\nservice_tier = \"ultrafast\"\n",
    );
    let chatgpt = |fast| seed(responses::CHATGPT_ID, Some(configured.clone()), fast);

    assert_eq!(tier(&chatgpt(None), SOL), (Some("ultrafast".into()), Some(Source::PerModel)));
    assert_eq!(tier(&chatgpt(None), FIVE), (Some("priority".into()), Some(Source::ProviderWide)));
    assert_eq!(
        tier(&chatgpt(Some(FastChoice::On)), SOL),
        (Some("ultrafast".into()), Some(Source::Fast))
    );
    assert_eq!(
        tier(&chatgpt(Some(FastChoice::On)), FIVE),
        (Some("priority".into()), Some(Source::Fast))
    );
    for model in [SOL, FIVE] {
        assert_eq!(
            tier(&chatgpt(Some(FastChoice::Off)), model),
            (Some("default".into()), Some(Source::Fast)),
            "`/fast off` asks for the ordinary tier explicitly, above any config"
        );
    }

    let bare = seed(responses::CHATGPT_ID, None, None);
    assert_eq!(tier(&bare, FIVE), (Some("priority".into()), Some(Source::ChatgptDefault)));
    assert_eq!(tier(&bare, SOL), (Some("ultrafast".into()), Some(Source::ChatgptDefault)));

    assert_eq!(
        tier(&seed(responses::ID, None, None), FIVE),
        (None, None),
        "the platform is sent no tier nobody asked for"
    );
    assert_eq!(
        tier(&seed(responses::ID, Some(table("service_tier = \"flex\"")), None), FIVE),
        (Some("flex".into()), Some(Source::ProviderWide))
    );
    let platform_sol_flex = table("[model.\"gpt-5.6-sol\"]\nservice_tier = \"flex\"\n");
    for model in [SOL, FIVE] {
        assert_eq!(
            tier(
                &seed(responses::ID, Some(platform_sol_flex.clone()), Some(FastChoice::On)),
                model
            ),
            (Some("priority".into()), Some(Source::Fast)),
            "`/fast on` on the platform is priority on every model, above a per-model entry"
        );
    }
}

#[test]
fn a_provider_that_does_not_speak_responses_resolves_nothing() {
    for provider in ["anthropic", "openrouter", "fake"] {
        let (resolved, source) =
            resolve(&seed(provider, Some(table(EVERY_KEY)), Some(FastChoice::On)), FIVE);

        assert_eq!(resolved, options::RequestOptions::default(), "{provider}");
        assert_eq!(source, None, "{provider}");
    }
    assert_eq!(fast_tier("anthropic", FIVE), None);
    assert_eq!(fast_tier(responses::CHATGPT_ID, SOL), Some("ultrafast"));
    assert_eq!(fast_tier(responses::ID, SOL), Some("priority"));
}

/// **AC-3**, the half W1 could not reach: `fast` is read as `priority` and the
/// resolved value never carries the word.
#[test]
fn fast_is_resolved_as_priority_and_never_reaches_the_request() {
    let (resolved, _) =
        resolve(&seed(responses::CHATGPT_ID, Some(table("service_tier = \"fast\"")), None), FIVE);

    assert_eq!(resolved.service_tier.as_deref(), Some("priority"));
    let rendered = serde_json::Value::Object(resolved.body).to_string();
    assert!(!rendered.contains("fast"), "{rendered}");
}

/// The W2 review's hardening, from the side that builds the value: the body
/// the wire splices carries no key its typed body writes itself, and no
/// directive either — whatever a table sets.
#[test]
fn the_body_carries_no_directive_and_no_key_the_wire_writes_itself() {
    for provider in [responses::CHATGPT_ID, responses::ID] {
        let (resolved, _) = resolve(&seed(provider, Some(table(EVERY_KEY)), None), FIVE);

        for key in
            OWN_KEYS.iter().chain(&["service_tier", "custom_tools", "server_tools", "text_format"])
        {
            assert!(!resolved.body.contains_key(*key), "{provider}: {key} reached the body");
        }
        assert!(
            resolved.body["reasoning"].get("summary").is_none(),
            "{provider}: `reasoning.summary` is a directive applied after the layers"
        );
        assert!(resolved.text_format.is_none(), "the resolver never sets a format");
    }
}

/// The completeness half of the same boundary: every key either id's loader
/// admits reaches the request exactly once — as a body leaf or as a
/// directive — so a field added to the decode struct and forgotten here is a
/// test failure rather than a setting that silently does nothing. Resolved
/// under the platform id because the resolver does not gate; the loader does.
#[test]
fn every_platform_key_reaches_the_request_exactly_once() {
    let (resolved, _) = resolve(&seed(responses::ID, Some(table(EVERY_KEY)), None), FIVE);

    let mut reached = Vec::new();
    for (key, value) in &resolved.body {
        match (key.as_str(), value) {
            ("reasoning" | "text" | "stream_options", serde_json::Value::Object(inner)) => {
                reached.extend(inner.keys().map(|leaf| format!("{key}.{leaf}")));
            }
            _ => reached.push(key.clone()),
        }
    }
    let directives = [
        ("service_tier", resolved.service_tier.is_some()),
        ("custom_tools", !resolved.custom_tools.is_empty()),
        ("server_tools", !resolved.server_tools.is_empty()),
        ("include", !resolved.include.is_empty()),
        ("reasoning.summary", resolved.reasoning_summary.is_some()),
    ];
    reached.extend(directives.iter().filter(|(_, set)| *set).map(|(key, _)| (*key).to_owned()));

    let unique: BTreeSet<&str> = reached.iter().map(String::as_str).collect();
    assert_eq!(unique.len(), reached.len(), "a key reached the request twice: {reached:?}");
    let vocabulary: BTreeSet<&str> =
        options::PLATFORM_ACCEPTED.iter().chain(options::SEAT_ACCEPTED).copied().collect();
    assert_eq!(unique, vocabulary);
}

#[test]
fn every_value_is_sent_in_the_wires_spelling_with_nothing_unset_among_it() {
    let (resolved, _) = resolve(&seed(responses::ID, Some(table(EVERY_KEY)), None), FIVE);

    assert_eq!(
        serde_json::Value::Object(resolved.body),
        json!({
            "reasoning": {"context": "all_turns", "mode": "pro"},
            "text": {"verbosity": "low"},
            "parallel_tool_calls": false,
            "stream_options": {"include_obfuscation": true},
            "tool_choice": "required",
            // An unset `compact_threshold` would be a `null` nobody configured;
            // this one is set, and the stripped shape is pinned just below.
            "context_management": [{"type": "compaction", "compact_threshold": 1000}],
            "client_metadata": {"origin": "ganja"},
            "access_programs": {"program": "research"},
            "max_output_tokens": 100,
            "max_tool_calls": 3,
            "prompt_cache_key": "k",
            "prompt_cache_retention": "24h",
            "prompt_cache_options": {"mode": "explicit", "ttl": "1h"},
            "temperature": 0.2,
            "top_p": 0.9,
            "top_logprobs": 2,
            "truncation": "auto",
            "safety_identifier": "s",
            "user": "u",
            "metadata": {"team": "core"},
            "moderation": {"model": "omni", "policy": "strict"},
        })
    );
    assert_eq!(resolved.service_tier.as_deref(), Some("flex"));
    assert_eq!(resolved.custom_tools, ["bash"]);
    assert_eq!(resolved.include, ["web_search_call.action.sources"]);
    assert_eq!(resolved.reasoning_summary.as_deref(), Some("concise"));
    assert_eq!(
        serde_json::to_string(&resolved.server_tools[0]).expect("a map serializes"),
        r#"{"search_context_size":"low","type":"web_search"}"#,
        "the entry passes through whole"
    );

    let (unset, _) = resolve(
        &seed(responses::ID, Some(table("context_management = [{ type = \"compaction\" }]")), None),
        FIVE,
    );
    assert_eq!(unset.body["context_management"], json!([{"type": "compaction"}]));
}

/// `reasoning.summary` is the platform's alone; the loader refuses it under
/// `chatgpt`, and the resolver would not send it there either.
#[test]
fn a_reasoning_summary_is_resolved_on_the_platform_alone() {
    let (seat, _) = resolve(&seed(responses::CHATGPT_ID, Some(table(EVERY_KEY)), None), FIVE);
    assert_eq!(seat.reasoning_summary, None);
}

/// The per-model overlay reaches every directive, not only the tier.
#[test]
fn a_per_model_entry_replaces_the_provider_wide_lists_for_that_model_alone() {
    let configured = table(
        "include = [\"web_search_call.action.sources\"]\ntext = { verbosity = \"low\" }\n\
         [model.\"gpt-5.6-sol\"]\ninclude = [\"message.output_text.logprobs\"]\n",
    );

    let (sol, _) = resolve(&seed(responses::ID, Some(configured.clone()), None), SOL);
    let (five, _) = resolve(&seed(responses::ID, Some(configured), None), FIVE);

    assert_eq!(sol.include, ["message.output_text.logprobs"]);
    assert_eq!(five.include, ["web_search_call.action.sources"]);
    assert_eq!(sol.body["text"], json!({"verbosity": "low"}), "an unset per-model key inherits");
}

/// What a compaction and a child carry of a resolved value: the tier and the
/// body, and none of the directives.
#[test]
fn the_summary_and_child_views_keep_the_tier_and_the_body_alone() {
    let (mut resolved, _) = resolve(&seed(responses::ID, Some(table(EVERY_KEY)), None), FIVE);
    resolved.text_format = Some(json!({"type": "json_schema"}));

    for view in [resolved.summary_view(), resolved.child_view()] {
        assert_eq!(
            view,
            options::RequestOptions {
                service_tier: resolved.service_tier.clone(),
                body: resolved.body.clone(),
                ..options::RequestOptions::default()
            }
        );
    }
}
