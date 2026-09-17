use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{DESCRIPTION, EvaluateTool, ID, line};
use crate::typesafe::tests::fixture::{self, Endpoint, answer};
use crate::typesafe::{Answer, DEFAULT_MODEL, PREVIEW_MODEL, Settings};
use crate::{Tool, ToolCtx, ToolError};

/// The key the tool is built with. Never read from the environment here: a
/// tool test that read the real `TYPESAFE_API_KEY` would pass or fail by
/// whose machine it ran on. What [`EvaluateTool::configured`] adds on top —
/// reading those variables — is pinned in `tests/evaluate_keys.rs`, which is
/// a binary of its own for exactly that reason.
const KEY: &str = "sk-typesafe-tool-fixture-0123456789";

/// A canned answer carrying one of each type, plus one this build cannot
/// read, so criterion 1b's four renderings come out of one exchange.
const ANSWERED: &str = r#"{"model":"jev-1.13.0","answers":{"dept":{"type":"choice","choice":"technical","probabilities":{"billing":0.08,"sales":0.07,"technical":0.85},"confidence":0.82},"frustration":{"type":"score","score":1.6,"legend":{"0":"calm","1":"cross","2":"furious"},"probabilities":{"0":0.05,"1":0.3,"2":0.65},"confidence":0.78},"urgent":{"type":"noul","noul":0.92},"x":{"type":"quanta","quanta":[0.1,0.9]}},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// A tool pointed at `endpoint`.
fn tool(endpoint: &Endpoint) -> EvaluateTool {
    let base = Settings::base_from(endpoint.base()).expect("a loopback base is accepted");

    EvaluateTool::against(
        Settings::new(KEY.to_owned(), base, DEFAULT_MODEL.to_owned())
            .expect("a checked base joins the endpoint path"),
    )
}

fn ctx() -> ToolCtx {
    ToolCtx::fixture(PathBuf::from("."))
}

/// The arguments criterion 1k's disclosure is computed from: an object state
/// with two keys, and two questions.
fn two_questions() -> serde_json::Value {
    serde_json::json!({
        "state": {"diff": "-a\n+b", "policy": "no force pushes"},
        "questions": {
            "risky": {"type": "noul", "instructions": "Is this risky?"},
            "area": {
                "type": "choice",
                "instructions": "Which area?",
                "criteria": {"docs": null, "code": "source files"}
            }
        }
    })
}

/// The smallest valid arguments, for tests about the exchange rather than
/// about the disclosure.
fn one_question() -> serde_json::Value {
    serde_json::json!({
        "state": "payouts have been failing",
        "questions": {"urgent": {"type": "noul", "instructions": "Urgent?"}}
    })
}

#[tokio::test]
async fn every_answer_type_renders_to_its_own_line_and_an_unknown_one_says_so() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let answered =
        tool(&endpoint).run(one_question(), &ctx()).await.expect("the canned answer parses");

    assert_eq!(
        answered.output,
        "dept: choice=technical p=0.85 confidence=0.82 (billing 0.08, sales 0.07)\n\
         frustration: score=1.60 of 0..2 confidence=0.78\n\
         urgent: noul=0.92\n\
         x: quanta (unrecognised answer type)"
    );
    // The **served** model, not the alias that was asked for.
    assert_eq!(answered.title, "4 answer(s) · jev-1.13.0 · 312 input tokens");
    assert_eq!(answered.metadata["model"], "jev-1.13.0");
    assert_eq!(answered.metadata["usage"]["input_tokens"], 312);
    assert!(answered.metadata["answers"]["urgent"]["noul"].as_f64().is_some());
    assert!(
        answered.metadata["latency_ms"].as_u64().is_some(),
        "a call reports what it cost: {}",
        answered.metadata
    );
}

#[test]
fn a_score_names_the_range_its_legend_actually_covers() {
    // Level keys are strings, so they are compared as numbers. Sorted as
    // text, `"10"` comes before `"2"` and a ten-level rubric would report a
    // range it does not have.
    let legend = (0..=10).map(|level| (level.to_string(), "x".to_owned())).collect();
    let answer =
        Answer::Score { score: 4.5, legend, probabilities: BTreeMap::new(), confidence: 0.5 };

    assert_eq!(line("mood", &answer), "mood: score=4.50 of 0..10 confidence=0.50");
}

#[tokio::test]
async fn the_dialog_title_names_the_host_and_the_byte_count_before_anything_else() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let tool = tool(&endpoint);
    let title = tool.describe(&two_questions());
    let (head, _) = title.split_once(" B · ").expect("the byte count comes early");
    let through_bytes = format!("{head} B");

    assert!(
        title.starts_with(&format!("{ID} → 127.0.0.1 · ")),
        "host first, so a wrapped row cannot hide it: {title}"
    );
    assert!(
        title.ends_with(" question(s) · jev-latest · state keys: diff, policy"),
        "and the shape of the state last: {title}"
    );
    assert!(
        through_bytes.chars().count() < 60,
        "the disclosure is inside the first 60 columns, so it cannot straddle \
         a row: {} columns of {through_bytes:?}",
        through_bytes.chars().count()
    );

    // The byte count is the request body's own, not an estimate: the cap and
    // the disclosure read one number.
    let bytes: usize = through_bytes
        .rsplit(' ')
        .nth(1)
        .and_then(|count| count.parse().ok())
        .expect("the title carries a byte count");
    assert!(bytes > 0);

    // A forwarded teammate dialog prefixes the sender's name, which shifts
    // the line right without moving the disclosure off the head of it.
    let forwarded = format!("reviewer · {title}");
    assert!(forwarded.contains("127.0.0.1"));
    assert!(forwarded.contains(&format!("{bytes} B")));
}

#[tokio::test]
async fn arguments_that_do_not_validate_describe_themselves_and_show_nothing() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let tool = tool(&endpoint);
    let refused = format!("{ID} → 127.0.0.1 · the arguments are not a valid request");

    for bad in [
        // Not a state at all.
        serde_json::json!({"state": 42, "questions": {"a": {"type": "noul", "instructions": "?"}}}),
        // A state, and a question the validator refuses.
        serde_json::json!({
            "state": "SENSITIVE PROJECT CONTENT",
            "questions": {"a b": {"type": "noul", "instructions": "?"}}
        }),
        // No questions at all.
        serde_json::json!({"state": "SENSITIVE PROJECT CONTENT", "questions": {}}),
        // Not even an object.
        serde_json::json!("nonsense"),
    ] {
        let said = tool.describe(&bad);

        assert_eq!(said, refused, "for {bad}");
        assert!(!said.contains("SENSITIVE"), "a refusal shows nothing from the arguments: {said}");
    }
    assert_eq!(endpoint.count(), 0, "describing opens no socket");
}

#[tokio::test]
async fn every_limit_refuses_the_call_before_a_socket_is_opened() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let tool = tool(&endpoint);
    let noul = serde_json::json!({"type": "noul", "instructions": "Urgent?"});
    let mut fifty_one = serde_json::Map::new();
    for index in 0..=crate::typesafe::MAX_QUESTIONS {
        fifty_one.insert(format!("q{index}"), noul.clone());
    }

    let refusals = [
        ("a numeric state", serde_json::json!({"state": 42, "questions": {"a": noul}})),
        ("no state at all", serde_json::json!({"questions": {"a": noul}})),
        ("no questions key", serde_json::json!({"state": "x"})),
        (
            "an argument the schema does not name",
            serde_json::json!({
                "state": "x", "questions": {"a": noul}, "temperature": 0.7
            }),
        ),
        ("one question past the cap", serde_json::json!({"state": "x", "questions": fifty_one})),
        ("no questions at all", serde_json::json!({"state": "x", "questions": {}})),
        ("an id with a space in it", serde_json::json!({"state": "x", "questions": {"a b": noul}})),
        (
            "a choice with one option",
            serde_json::json!({
                "state": "x",
                "questions": {"d": {"type": "choice", "instructions": "?", "criteria": {"only": null}}}
            }),
        ),
        (
            "a score with one level",
            serde_json::json!({
                "state": "x",
                "questions": {"m": {"type": "score", "instructions": "?", "criteria": ["calm"]}}
            }),
        ),
        (
            "a state over the body cap",
            serde_json::json!({
                "state": "p".repeat(crate::typesafe::MAX_BODY),
                "questions": {"a": noul}
            }),
        ),
        (
            "an unknown key inside a question",
            serde_json::json!({
                "state": "x",
                "questions": {"a": {"type": "noul", "instructions": "?", "nope": 1}}
            }),
        ),
    ];

    for (what, args) in refusals {
        let refused = tool.run(args, &ctx()).await;

        assert!(
            matches!(refused, Err(ToolError::InvalidArgs(_))),
            "{what} is refused as bad arguments: {refused:?}"
        );
    }
    assert_eq!(endpoint.count(), 0, "not one refusal reached the vendor");
}

#[test]
fn the_description_stays_inside_its_budget_and_keeps_every_sentence_that_earns_its_place() {
    // Every request of a configured session carries this, so the budget is
    // the point rather than a formality.
    assert!(DESCRIPTION.len() <= 1_200, "{} bytes", DESCRIPTION.len());

    for pinned in [
        // The three primitives, by name.
        "`noul` answers a yes/no question with the probability of yes",
        "`choice` picks one of the options you name",
        "`score` places the content on ordered levels you define",
        // One narrow judgement per question, asked together.
        "Each question is one narrow judgement",
        "Ask independent questions together in one call",
        // Ids are not sent, so the instructions must stand alone.
        "id is not sent to the model",
        "must stand alone",
        // Where the evidence goes and how it is referred to.
        "Put the evidence in `state` as named fields",
        "by backticked path",
        // What an undecided answer looks like.
        "A `noul` near 0.5 is undecided, not a no.",
        // And the sentence the whole tool is gated on.
        "is a probability, not a fact",
        "never make it the sole ground for an irreversible action",
    ] {
        assert!(DESCRIPTION.contains(pinned), "the description no longer says: {pinned}");
    }
}

#[tokio::test]
async fn the_schema_states_the_shape_rather_than_leaving_it_to_the_model() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let schema = serde_json::to_value(tool(&endpoint).schema()).expect("a schema is JSON");

    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["additionalProperties"], false,
        "an argument the tool does not take is refused rather than ignored"
    );
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("a required list")
        .iter()
        .filter_map(|name| name.as_str())
        .collect();
    assert!(required.contains(&"state"), "got {required:?}");
    assert!(required.contains(&"questions"), "got {required:?}");
    assert!(!required.contains(&"model"), "the model is optional: {required:?}");

    // A `state` typed as the vendor types it, rather than schemars' always-
    // true schema for a bare `Value`.
    let state = &schema["properties"]["state"];
    assert!(
        state.get("$ref").is_some() || state.get("anyOf").is_some(),
        "the state names its own shape: {state}"
    );

    // A1: the model argument is a free string whose description names both
    // aliases, since a model never told an alias exists cannot ask for it.
    let model = &schema["properties"]["model"];
    let described = model["description"].as_str().expect("the model argument is described");
    assert!(described.contains(DEFAULT_MODEL), "{described}");
    assert!(described.contains(PREVIEW_MODEL), "{described}");
}

#[tokio::test]
async fn the_model_a_call_names_reaches_both_the_wire_and_the_dialog() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;
    let tool = tool(&endpoint);
    let mut args = one_question();
    args["model"] = serde_json::json!(PREVIEW_MODEL);

    assert!(
        tool.describe(&args).contains(&format!(" · {PREVIEW_MODEL} · ")),
        "the dialog names what would be sent: {}",
        tool.describe(&args)
    );

    tool.run(args, &ctx()).await.expect("the canned answer parses");

    assert!(
        endpoint.first().contains(&format!(r#""model":"{PREVIEW_MODEL}""#)),
        "the alias reaches the wire unchanged: {}",
        endpoint.first()
    );

    // Absent, it is the session's configured default rather than a literal
    // this module spells a second time.
    let plain = one_question();
    assert!(tool.describe(&plain).contains(&format!(" · {DEFAULT_MODEL} · ")));

    tool.run(plain, &ctx()).await.expect("the canned answer parses");
    assert!(endpoint.requests()[1].contains(&format!(r#""model":"{DEFAULT_MODEL}""#)));
}

#[tokio::test]
async fn a_vendor_failure_reaches_the_model_as_a_sentence_it_can_act_on() {
    let endpoint = fixture::serve(fixture::canned(429, "{}")).await;
    let refused = tool(&endpoint).run(one_question(), &ctx()).await;

    let Err(ToolError::Failed(message)) = refused else {
        panic!("an unavailable vendor is a failure the model reads: {refused:?}");
    };
    assert!(message.contains("continue without this judgement"), "{message}");
    assert_eq!(endpoint.count(), 1, "and it was not retried");
}

#[tokio::test]
async fn the_tool_is_named_what_the_permission_rules_gate() {
    let endpoint = fixture::serve(answer(ANSWERED)).await;

    assert_eq!(tool(&endpoint).id(), "evaluate");
    assert_eq!(ID, "evaluate");
    assert!(
        ganja_permission::permission::ASK_BY_DEFAULT.contains(&ID),
        "a tool that sends project content to a third party asks first"
    );
}
