use serde_json::json;

use super::{Answer, CallToolResult, Content, ToolCall, answer, reply, wrapped};
use crate::tool::ToolDefinition;

fn roster() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "ganja_ping".to_owned(),
        description: "Answers pong. A probe.".to_owned(),
        schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    }]
}

/// The bytes the recording proves accepted, modulo the ids: run 1's idx3 —
/// and one key more since `i5oi`, `tools.listChanged`, which no recorded run
/// declared.
#[test]
fn initialize_is_answered_with_the_clis_own_protocol_version_echoed() {
    let asked = json!({
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "claude-code", "version": "2.1.266"},
        },
        "jsonrpc": "2.0",
        "id": 0,
    });

    let Answer::Reply(sent) = answer(&asked, &roster(), "0.1.0") else {
        panic!("initialize is answered here");
    };

    assert_eq!(
        sent,
        json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "ganja", "version": "0.1.0"},
            },
        })
    );
}

/// Declaring `capabilities.tools` is what makes the CLI ask for a roster at
/// all, so a server that answered with an empty object would be one nothing
/// is ever called on.
#[test]
fn the_initialize_answer_declares_the_capability_that_earns_a_roster_request() {
    let Answer::Reply(sent) = answer(&json!({"method": "initialize", "id": 0}), &roster(), "0.1.0")
    else {
        panic!("initialize is answered here");
    };

    assert!(sent["result"]["capabilities"].get("tools").is_some());
}

/// `i5oi`: the roster may move while the process lives, and the server says
/// so up front — the declaration the CLI subscribes to `list_changed` on — and
/// then says it with a notification that carries no id, because JSON-RPC
/// answers a notification with nothing.
#[test]
fn the_server_declares_a_roster_that_can_change_and_announces_it_without_an_id() {
    let Answer::Reply(sent) = answer(&json!({"method": "initialize", "id": 0}), &roster(), "0.1.0")
    else {
        panic!("initialize is answered here");
    };
    assert_eq!(sent["result"]["capabilities"]["tools"]["listChanged"], json!(true), "{sent}");

    let announced = super::list_changed();
    assert_eq!(announced, json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
    assert!(announced.get("id").is_none(), "a notification carries no id: {announced}");
}

#[test]
fn an_initialize_naming_no_version_is_answered_under_the_one_the_recording_holds() {
    let Answer::Reply(sent) = answer(&json!({"method": "initialize", "id": 0}), &roster(), "0.1.0")
    else {
        panic!("initialize is answered here");
    };

    assert_eq!(sent["result"]["protocolVersion"], super::DEFAULT_PROTOCOL);
}

/// A JSON-RPC notification earns no response of its own; the CLI's own
/// envelope still wants the empty success, which is what run 1's idx8 shows.
#[test]
fn a_notification_is_answered_with_the_empty_success() {
    assert_eq!(
        answer(
            &json!({"method": "notifications/initialized", "jsonrpc": "2.0"}),
            &roster(),
            "0.1.0"
        ),
        Answer::Empty
    );
}

/// Names go out **bare**. The CLI prefixes what this side declares, and a
/// `tools/call` comes back bare again (M6) — so the roster is the registry's
/// own names and a call is matched back by its `_meta` correlator.
#[test]
fn the_roster_is_declared_under_the_registrys_own_bare_names() {
    let Answer::Reply(sent) = answer(&json!({"method": "tools/list", "id": 1}), &roster(), "0.1.0")
    else {
        panic!("tools/list is answered here");
    };

    assert_eq!(
        sent,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"tools": [{
                "name": "ganja_ping",
                "description": "Answers pong. A probe.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            }]},
        })
    );
}

#[test]
fn the_roster_keeps_the_order_the_engine_advertised() {
    let mut tools = roster();
    tools.push(ToolDefinition {
        name: "bash".to_owned(),
        description: "runs a command".to_owned(),
        schema: json!({}),
    });

    let Answer::Reply(sent) = answer(&json!({"method": "tools/list", "id": 1}), &tools, "0.1.0")
    else {
        panic!("tools/list is answered here");
    };
    let listed: Vec<&str> = sent["result"]["tools"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();

    assert_eq!(listed, ["ganja_ping", "bash"]);
}

#[test]
fn ping_is_answered_with_the_empty_result() {
    let Answer::Reply(sent) = answer(&json!({"method": "ping", "id": 3}), &roster(), "0.1.0")
    else {
        panic!("ping is answered here");
    };

    assert_eq!(sent, json!({"jsonrpc": "2.0", "id": 3, "result": {}}));
}

/// A method this server does not implement is refused **by name**, so a
/// person reading the CLI's log can see what was asked for.
#[test]
fn a_method_this_server_does_not_have_is_refused_naming_it() {
    let Answer::Reply(sent) =
        answer(&json!({"method": "resources/list", "id": 4}), &roster(), "0.1.0")
    else {
        panic!("an unknown method is answered here");
    };

    assert_eq!(sent["error"]["code"], -32601);
    assert!(
        sent["error"]["message"].as_str().is_some_and(|said| said.contains("resources/list")),
        "the method is named: {}",
        sent["error"]["message"]
    );
    assert_eq!(sent["id"], 4);
}

/// `tools/call` is **not** answered here. Nothing in this crate runs a tool,
/// so the call is handed back for the bridge to resolve against the engine's
/// own finished part.
#[test]
fn a_tools_call_is_handed_back_rather_than_answered() {
    let asked = json!({
        "method": "tools/call",
        "params": {
            "name": "ganja_ping",
            "arguments": {},
            "_meta": {"claudecode/toolUseId": "toolu_012w", "progressToken": 2},
        },
        "jsonrpc": "2.0",
        "id": 2,
    });

    assert_eq!(
        answer(&asked, &roster(), "0.1.0"),
        Answer::Call(ToolCall {
            id: json!(2),
            name: "ganja_ping".to_owned(),
            tool_use_id: Some("toolu_012w".to_owned()),
        })
    );
}

#[test]
fn a_tools_call_with_no_meta_carries_no_correlator_and_is_matched_some_other_way() {
    let asked = json!({
        "method": "tools/call",
        "params": {"name": "ganja_ping", "arguments": {}},
        "id": 2,
    });

    assert_eq!(
        answer(&asked, &roster(), "0.1.0"),
        Answer::Call(ToolCall { id: json!(2), name: "ganja_ping".to_owned(), tool_use_id: None })
    );
}

/// Run 1's idx25, minus the second block the allow path no longer sends.
#[test]
fn a_call_result_serializes_to_the_shape_the_recording_proves_accepted() {
    let result = CallToolResult { content: vec![Content::text("pong")], is_error: false };

    assert_eq!(
        reply(&json!(2), &result),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"content": [{"type": "text", "text": "pong"}], "isError": false},
        })
    );
}

#[test]
fn a_failed_call_says_so_on_the_result_rather_than_as_a_refusal() {
    let result = CallToolResult { content: vec![Content::text("no such file")], is_error: true };

    assert_eq!(reply(&json!(2), &result)["result"]["isError"], true);
}

#[test]
fn a_reply_is_wrapped_the_way_the_clis_own_envelope_carries_it() {
    assert_eq!(
        wrapped(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
        json!({"mcp_response": {"jsonrpc": "2.0", "id": 1, "result": {}}})
    );
}

/// The one name this side declares, and the one an `mcp_message` must be
/// addressed to.
#[test]
fn the_server_is_named_ganja() {
    assert_eq!(super::SERVER, "ganja");
}
