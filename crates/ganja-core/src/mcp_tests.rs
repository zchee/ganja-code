use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use super::{
    Server, Servers, Status, catalog, decoded_len, force_object, render, sanitize, tool_name,
};
use crate::{
    config::{MCP_CALL_TIMEOUT, MCP_LIST_TIMEOUT, McpServer},
    tool::ToolError,
};

/// A tool definition as a server would have listed it.
fn listed(name: &str) -> rmcp::model::Tool {
    rmcp::model::Tool::new(
        name.to_owned(),
        "does a thing".to_owned(),
        Arc::new(serde_json::Map::new()),
    )
}

#[test]
fn a_tool_is_named_for_its_server_and_itself() {
    let cases = [
        ("github", "create_issue", "mcp__github__create_issue"),
        // Hyphens survive the sanitizer; dots and spaces do not.
        (
            "my.special-server",
            "tool-a",
            "mcp__my_special-server__tool-a",
        ),
        (
            "my.special-server",
            "tool.b",
            "mcp__my_special-server__tool_b",
        ),
        ("a b", "c/d", "mcp__a_b__c_d"),
        // Not ASCII, so not kept: one replacement per character.
        ("héllo", "wörld", "mcp__h_llo__w_rld"),
    ];

    for (server, tool, expected) in cases {
        assert_eq!(tool_name(server, tool), expected, "{server}/{tool}");
    }
}

#[test]
fn sanitizing_touches_only_what_a_name_may_not_hold() {
    assert_eq!(sanitize("Abc_09-xyz"), "Abc_09-xyz");
    assert_eq!(sanitize("a.b:c d/e"), "a_b_c_d_e");
}

#[test]
fn a_schema_is_forced_to_an_object_a_provider_will_take() {
    let forced = force_object(
        json!({ "type": "string", "description": "kept" })
            .as_object()
            .expect("the fixture is an object"),
    );

    assert_eq!(
        serde_json::Value::Object(forced),
        json!({
            "type": "object",
            "description": "kept",
            "properties": {},
            "additionalProperties": false,
        })
    );

    // An object that already had properties keeps them.
    let forced = force_object(
        json!({ "type": "object", "properties": { "path": { "type": "string" } } })
            .as_object()
            .expect("the fixture is an object"),
    );
    assert_eq!(forced["properties"]["path"]["type"], json!("string"));
    assert_eq!(forced["additionalProperties"], json!(false));
}

/// Two tools whose sanitized names collide: the first one listed keeps the
/// name and the second is not registered at all.
#[test]
fn two_tools_that_sanitize_to_one_name_leave_only_the_first() {
    let one = "a.b".to_owned();
    let defs = [listed("tool.x"), listed("tool_x"), listed("other")];

    let names: Vec<String> = catalog(&[(&one, defs.as_slice())])
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();

    assert_eq!(
        names,
        vec!["mcp__a_b__tool_x".to_owned(), "mcp__a_b__other".to_owned(),]
    );
}

/// A collision across two servers is decided the same way, and the order
/// servers contribute in is the sorted one a rebuild always sees.
#[test]
fn servers_contribute_in_sorted_order_and_the_earlier_one_keeps_the_name() {
    let first = "alpha.one".to_owned();
    let second = "beta".to_owned();
    let shared = [listed("run")];
    let alias = [listed("run"), listed("stop")];

    let catalog = catalog(&[(&first, shared.as_slice()), (&second, alias.as_slice())]);
    let names: Vec<String> = catalog.iter().map(|(id, _, _)| id.clone()).collect();

    assert_eq!(
        names,
        vec![
            "mcp__alpha_one__run".to_owned(),
            "mcp__beta__run".to_owned(),
            "mcp__beta__stop".to_owned(),
        ]
    );

    // `alpha.one` and `alpha_one` are one name after sanitization, so now
    // the two `run` tools really do collide and only the first survives.
    let clash = "alpha_one".to_owned();
    let names = catalog_names(&[(&first, shared.as_slice()), (&clash, alias.as_slice())]);
    assert_eq!(
        names,
        vec![
            "mcp__alpha_one__run".to_owned(),
            "mcp__alpha_one__stop".to_owned(),
        ]
    );
}

/// The names [`catalog`] would register, for a test that only cares about
/// those.
fn catalog_names(listings: &[(&String, &[rmcp::model::Tool])]) -> Vec<String> {
    catalog(listings).into_iter().map(|(id, _, _)| id).collect()
}

#[test]
fn a_configured_timeout_governs_calls_and_listings_but_not_the_connect() {
    let entry: McpServer = serde_json::from_value(json!({
        "type": "local",
        "command": ["echo"],
        "timeout": 1234,
    }))
    .expect("the fixture entry parses");

    assert_eq!(entry.timeout(MCP_CALL_TIMEOUT), 1234);
    assert_eq!(entry.timeout(MCP_LIST_TIMEOUT), 1234);

    let silent: McpServer = serde_json::from_value(json!({
        "type": "local",
        "command": ["echo"],
    }))
    .expect("the fixture entry parses");

    assert_eq!(silent.timeout(MCP_CALL_TIMEOUT), MCP_CALL_TIMEOUT);
    assert_eq!(silent.timeout(MCP_LIST_TIMEOUT), MCP_LIST_TIMEOUT);
}

#[test]
fn an_error_result_carries_the_servers_own_words() {
    let mut result = rmcp::model::CallToolResult::success(vec![
        rmcp::model::ContentBlock::text("   "),
        rmcp::model::ContentBlock::text("the repository is archived"),
    ]);
    result.is_error = Some(true);

    let error = render(
        "mcp__github__create_issue",
        result,
        crate::tool::truncate::MAX_CHARS,
    )
    .expect_err("isError is an error");
    assert!(
        matches!(&error, ToolError::Failed(text) if text == "the repository is archived"),
        "{error}"
    );
}

#[test]
fn an_error_result_with_nothing_to_say_still_says_something() {
    let mut result = rmcp::model::CallToolResult::success(Vec::new());
    result.is_error = Some(true);

    let error = render("mcp__x__y", result, crate::tool::truncate::MAX_CHARS)
        .expect_err("isError is an error");
    assert!(
        matches!(&error, ToolError::Failed(text) if text == super::UNSPOKEN_ERROR),
        "{error}"
    );
}

#[test]
fn a_structured_only_result_becomes_one_json_block() {
    let mut result = rmcp::model::CallToolResult::success(Vec::new());
    result.structured_content = Some(json!({ "count": 2 }));

    let output = render("mcp__x__y", result, crate::tool::truncate::MAX_CHARS)
        .expect("a structured answer is an answer");
    assert_eq!(output.output, r#"{"count":2}"#);
}

#[test]
fn binary_content_is_described_rather_than_carried() {
    let result = rmcp::model::CallToolResult::success(vec![
        rmcp::model::ContentBlock::text("here it is"),
        // Nine bytes, base64-encoded.
        rmcp::model::ContentBlock::image("MTIzNDU2Nzg5", "image/png"),
    ]);

    let output = render("mcp__x__y", result, crate::tool::truncate::MAX_CHARS)
        .expect("an image answer is an answer");
    assert_eq!(
        output.output,
        "here it is\n[binary MCP content omitted: image/png, 9 bytes]"
    );
}

#[test]
fn a_base64_length_is_counted_rather_than_decoded() {
    let cases = [
        // Padded, the common wire form.
        ("", 0),
        ("MTIz", 3),
        ("MTI=", 2),
        ("MQ==", 1),
        // Unpadded (RFC 4648 §3.2), the same "f"/"fo"/"foob"/"fooba"
        // vectors as above with their `=` stripped: a length that is not
        // a multiple of four is a partial final group, not a malformed
        // string.
        ("Zg", 1),
        ("Zm8", 2),
        ("Zm9vYg", 4),
        ("Zm9vYmE", 5),
    ];
    for (encoded, expected) in cases {
        assert_eq!(decoded_len(encoded), expected, "{encoded:?}");
    }
}

#[test]
fn a_disabled_server_is_disabled_before_anything_is_dialled() {
    let config = BTreeMap::from([
        (
            "off".to_owned(),
            serde_json::from_value(json!({
                "type": "local",
                "command": ["never-run"],
                "enabled": false,
            }))
            .expect("the fixture entry parses"),
        ),
        (
            "on".to_owned(),
            serde_json::from_value(json!({ "type": "local", "command": ["also-never"] }))
                .expect("the fixture entry parses"),
        ),
    ]);
    let servers = Servers::new(config, std::path::Path::new("/"));

    assert_eq!(
        servers.status(),
        BTreeMap::from([("off".to_owned(), Status::Disabled)]),
        "a server nothing has tried yet has no status to report"
    );
}

#[test]
fn instructions_come_only_from_a_server_that_lent_a_tool() {
    let servers = Servers::new(BTreeMap::new(), std::path::Path::new("/"));
    {
        let mut state = servers.state();
        // Connected, but lent nothing: nothing to instruct about.
        state.insert(
            "quiet".to_owned(),
            Server {
                status: Some(Status::Connected),
                client: None,
                defs: Vec::new(),
                instructions: Some("ignored".to_owned()),
                group: None,
                ever_connected: true,
            },
        );
    }

    assert_eq!(servers.instructions(), None);
}
