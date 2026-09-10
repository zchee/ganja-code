use serde_json::json;

use super::{
    CarriedOn, MODEL_FACING_PREFIX, Pending, Permission, cancelled, model_facing_name, part_for,
    registry_name, resolve,
};
use crate::protocol::{Message, Part, PartBody, PartId, ToolState};

fn pending(tool_use_id: &str, name: &str) -> Pending {
    Pending {
        request_id: Some(format!("req-{tool_use_id}")),
        tool_use_id: tool_use_id.to_owned(),
        name: name.to_owned(),
        input: json!({"path": "/f"}),
        call_request_id: None,
        call_rpc_id: None,
    }
}

/// One assistant message carrying a finished call.
fn finished(call_id: &str, state: ToolState) -> Message {
    let mut message = Message::assistant("claude-opus-5");
    message.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::Tool { call_id: call_id.to_owned(), tool: "read".to_owned(), state },
    });

    message
}

fn ran(output: &str) -> ToolState {
    ToolState::Completed {
        input: json!({"path": "/f"}),
        output: output.to_owned(),
        title: String::new(),
        metadata: json!({}),
        started: 0,
        completed: 0,
    }
}

fn broke(error: &str) -> ToolState {
    ToolState::Error {
        input: json!({"path": "/f"}),
        error: error.to_owned(),
        started: 0,
        completed: 0,
    }
}

fn steer(text: &str) -> Message {
    let mut message = Message::user(text);
    message.id = crate::protocol::MessageId::from("m-steer".to_owned());

    message
}

#[test]
fn a_call_that_ran_is_allowed_and_its_output_travels_as_the_one_block() {
    let resolved =
        resolve(&[pending("toolu_1", "read")], &[finished("toolu_1", ran("contents"))], None)
            .expect("the part is there");

    assert_eq!(resolved.answers.len(), 1);
    assert_eq!(
        resolved.answers[0].permission,
        Permission::Allow { updated_input: json!({"path": "/f"}) }
    );
    let result = resolved.answers[0].result.as_ref().expect("an allowed call is called");
    assert_eq!(result.content.len(), 1, "one block, the part's output, byte-identical");
    assert_eq!(result.content[0].text, "contents");
    assert!(!result.is_error);
}

/// **Not** a refusal: an unknown tool, bad arguments or a command that exited
/// non-zero is a call that ran and failed, which is what the model should
/// read.
#[test]
fn a_call_that_ran_and_failed_is_allowed_and_its_failure_is_the_tools_own_answer() {
    let resolved =
        resolve(&[pending("toolu_1", "read")], &[finished("toolu_1", broke("no such file"))], None)
            .expect("the part is there");

    assert!(matches!(resolved.answers[0].permission, Permission::Allow { .. }));
    let result = resolved.answers[0].result.as_ref().expect("a failed call still ran");
    assert!(result.is_error);
    assert_eq!(result.content[0].text, "no such file");
}

/// The three refusal sentences, spelled as **literals** here (D552's Dv-8):
/// a test built from the constants passes any reword, and a reword is what
/// silently changes what the CLI is told a call did.
#[test]
fn each_of_the_three_refusal_sentences_is_answered_deny_and_is_never_called() {
    for sentence in [
        "The user rejected permission to use this specific tool call.",
        "The user has specified a rule which prevents you from using this specific tool call. \
         Here are some of the relevant rules read: ask",
        "A PreToolUse hook refused this tool call: the repo is frozen",
    ] {
        let resolved =
            resolve(&[pending("toolu_1", "read")], &[finished("toolu_1", broke(sentence))], None)
                .expect("the part is there");

        assert_eq!(
            resolved.answers[0].permission,
            Permission::Deny { message: sentence.to_owned() },
            "{sentence}"
        );
        assert!(
            resolved.answers[0].result.is_none(),
            "a denied call is never called, so it has no result to answer with"
        );
    }
}

/// There is no field for it on the answer, and the serialized bytes carry no
/// such key — so an `allow` records no CLI-side grant and the next ask for
/// the same tool asks again.
#[test]
fn an_allow_carries_no_updated_permissions_key() {
    let payload = Permission::Allow { updated_input: json!({}) }.payload();

    assert_eq!(payload, json!({"behavior": "allow", "updatedInput": {}}));
    assert!(payload.get("updatedPermissions").is_none());
    assert!(!payload.to_string().contains("updatedPermissions"));
}

#[test]
fn a_deny_carries_the_message_and_nothing_else() {
    let payload = Permission::Deny { message: "no".to_owned() }.payload();

    assert_eq!(payload, json!({"behavior": "deny", "message": "no"}));
}

// ------------------------------------------------ the mid-turn message

/// On the allow path the steer is **deferred**: the answer carries the part's
/// output alone. The channel was measured working and useless — the CLI
/// delivered both blocks and the model named the second as injection in the
/// reply the person reads.
#[test]
fn a_steer_that_arrives_while_an_allowed_tool_ran_appears_nowhere_in_the_answer() {
    let steer = steer("change of plan: say lantern");
    let resolved =
        resolve(&[pending("toolu_1", "read")], &[finished("toolu_1", ran("pong"))], Some(&steer))
            .expect("the part is there");

    let result = resolved.answers[0].result.as_ref().expect("an allowed call is called");
    assert_eq!(result.content.len(), 1);
    assert_eq!(result.content[0].text, "pong");
    assert!(!result.content[0].text.contains("lantern"));
    assert!(!serde_json::to_string(result).expect("it serializes").contains("while the tool ran"));

    assert_eq!(resolved.carried_on, Some(CarriedOn::Deferred));
    assert_eq!(resolved.carried_id, None, "it stays owed, so it is not a write");
}

/// On the deny path the carry stays: a `deny.message` is text the model reads
/// as the tool's own refusal rather than as a user's voice.
#[test]
fn a_steer_that_arrives_while_a_refused_tool_was_declined_rides_the_deny_message() {
    let steer = steer("change of plan: say lantern");
    let refusal = "The user rejected permission to use this specific tool call.";
    let resolved = resolve(
        &[pending("toolu_1", "read")],
        &[finished("toolu_1", broke(refusal))],
        Some(&steer),
    )
    .expect("the part is there");

    let Permission::Deny { message } = &resolved.answers[0].permission else {
        panic!("a refusal is denied");
    };
    assert!(message.starts_with(refusal));
    assert!(message.ends_with("[User, while the tool ran] change of plan: say lantern"));
    assert!(message.contains("\n\n"), "a blank line separates the refusal from the carry");

    assert_eq!(resolved.carried_on, Some(CarriedOn::DenyMessage));
    assert_eq!(
        resolved.carried_id.as_deref(),
        Some("m-steer"),
        "the deny path's carry IS a write, so its id joins `sent`"
    );
}

#[test]
fn a_steer_carried_across_two_denied_asks_is_carried_exactly_once() {
    let steer = steer("stop");
    let refusal = "The user rejected permission to use this specific tool call.";
    let resolved = resolve(
        &[pending("toolu_1", "read"), pending("toolu_2", "read")],
        &[finished("toolu_1", broke(refusal)), finished("toolu_2", broke(refusal))],
        Some(&steer),
    )
    .expect("both parts are there");

    let carried: Vec<bool> = resolved
        .answers
        .iter()
        .map(|answer| match &answer.permission {
            Permission::Deny { message } => message.contains("while the tool ran"),
            Permission::Allow { .. } => false,
        })
        .collect();

    assert_eq!(carried, [true, false], "on the first answer written, and no other");
}

#[test]
fn a_steer_is_deferred_when_one_ask_is_denied_and_the_first_is_allowed() {
    let steer = steer("stop");
    let refusal = "The user rejected permission to use this specific tool call.";
    let resolved = resolve(
        &[pending("toolu_1", "read"), pending("toolu_2", "read")],
        &[finished("toolu_1", ran("pong")), finished("toolu_2", broke(refusal))],
        Some(&steer),
    )
    .expect("both parts are there");

    // The first deny written is the second answer, and it is the one that
    // carries.
    assert_eq!(resolved.carried_on, Some(CarriedOn::DenyMessage));
    assert!(resolved.answers[0].result.as_ref().expect("allowed").content.len() == 1);
}

// ------------------------------------------------------- the error arms

/// A keyed match with pendings and no results means the engine and this wire
/// disagree about what has been run. **Never a new turn**: opening one on
/// that disagreement would run something twice.
#[test]
fn a_parked_ask_with_no_part_is_an_error_naming_the_id() {
    let missing = resolve(&[pending("toolu_1", "read")], &[], None).expect_err("no part");

    assert_eq!(missing, "toolu_1");
}

#[test]
fn a_part_the_engine_has_not_finished_with_reads_the_same_way_as_a_missing_one() {
    for unfinished in [
        ToolState::Pending { input: None },
        ToolState::Running { input: json!({}), metadata: json!({}), started: 0 },
    ] {
        let error =
            resolve(&[pending("toolu_1", "read")], &[finished("toolu_1", unfinished)], None)
                .expect_err("an unfinished part is not an answer");

        assert_eq!(error, "toolu_1");
    }
}

#[test]
fn the_answers_come_back_in_the_order_the_asks_were_parked() {
    let resolved = resolve(
        &[pending("toolu_1", "read"), pending("toolu_2", "bash")],
        &[finished("toolu_2", ran("second")), finished("toolu_1", ran("first"))],
        None,
    )
    .expect("both parts are there");

    let ids: Vec<&str> =
        resolved.answers.iter().map(|answer| answer.tool_use_id.as_str()).collect();
    assert_eq!(ids, ["toolu_1", "toolu_2"]);
    assert_eq!(resolved.answers[0].result.as_ref().expect("allowed").content[0].text, "first");
}

// ------------------------------------------------------------- the names

/// The CLI prefixes what this side declares, so a `can_use_tool`'s
/// `tool_name` arrives prefixed and a `tools/call`'s `params.name` does not.
#[test]
fn a_model_facing_name_is_stripped_back_to_the_registrys_own() {
    assert_eq!(registry_name("mcp__ganja__ganja_ping"), "ganja_ping");
    assert_eq!(model_facing_name("ganja_ping"), "mcp__ganja__ganja_ping");
    assert_eq!(MODEL_FACING_PREFIX, "mcp__ganja__");
}

#[test]
fn a_bare_name_survives_the_strip_unchanged() {
    assert_eq!(registry_name("ganja_ping"), "ganja_ping");
}

/// D462 keeps parts in call order, so a linear walk finds the right one.
#[test]
fn a_part_is_found_by_the_call_id_every_side_of_the_wire_shares() {
    let turn = [finished("toolu_1", ran("first")), finished("toolu_2", ran("second"))];

    assert!(
        matches!(part_for("toolu_2", &turn), Some(ToolState::Completed { output, .. }) if output == "second")
    );
    assert!(part_for("toolu_3", &turn).is_none());
}

/// The CLI is never left holding a question ganja will not answer: an
/// unanswered `can_use_tool` does not time out, and a cancel that simply
/// stopped reading would wedge the process for the life of the session.
#[test]
fn a_cancel_answers_a_parked_ask_rather_than_leaving_it_open() {
    assert_eq!(cancelled(), Permission::Deny { message: "cancelled".to_owned() });
}
