use std::collections::HashMap;
use std::path::Path;

use super::*;

/// A lookup standing in for the process environment, so the refusals below
/// are exercised without `unsafe` and without one test's exports leaking into
/// the next one's.
fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> =
        pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect();

    move |variable: &str| map.get(variable).cloned()
}

/// A spike turned on with nothing else said.
fn plain() -> Spike {
    Spike::read(&env(&[(ENABLE_ENV, "1")])).expect("a bare enable is readable").expect("enabled")
}

/// The top-level field numbers a message went out with, in wire order.
///
/// A second copy of `request_tests.rs`'s walker rather than a shared one:
/// this whole file is deleted in W3a, and a helper it had hoisted into the
/// shipped tests would have to be un-hoisted then.
fn field_numbers(bytes: &[u8]) -> Vec<u32> {
    let varint = |bytes: &[u8]| -> (u64, usize) {
        let mut value = 0u64;
        let mut shift = 0u32;
        for (index, byte) in bytes.iter().enumerate() {
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return (value, index + 1);
            }
            shift += 7;
        }
        panic!("a truncated varint");
    };

    let mut numbers = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let (tag, read) = varint(&bytes[cursor..]);
        cursor += read;
        numbers.push(u32::try_from(tag >> 3).expect("a field number fits"));
        match tag & 7 {
            0 => cursor += varint(&bytes[cursor..]).1,
            1 => cursor += 8,
            2 => {
                let (len, read) = varint(&bytes[cursor..]);
                cursor += read + usize::try_from(len).expect("a length fits");
            }
            other => panic!("a wire type these messages do not use: {other}"),
        }
    }

    numbers
}

#[test]
fn the_spike_is_off_unless_its_flag_says_otherwise() {
    assert!(Spike::read(&env(&[])).expect("an empty environment is readable").is_none());
    assert!(
        Spike::read(&env(&[(ENABLE_ENV, "0")])).expect("an explicit no is readable").is_none(),
        "a flag set to zero is a flag saying no, not a flag saying something unreadable"
    );
    assert!(
        Spike::read(&env(&[(DELAY_ENV, "25"), (SCHEMA_ENV, "3")]))
            .expect("the other flags alone are readable")
            .is_none(),
        "every other flag is read only once the enable says yes"
    );
}

#[test]
fn a_flag_spelled_as_neither_a_yes_nor_a_no_refuses_the_run() {
    let refusal = Spike::read(&env(&[(ENABLE_ENV, "yes")])).expect_err("`yes` is not read");
    let ProviderError::Transport(message) = refusal else { panic!("a transport refusal") };
    assert!(message.contains(ENABLE_ENV), "the refusal names the variable: {message}");
    assert!(message.contains("\"yes\""), "and what it was set to: {message}");
}

#[test]
fn a_delay_that_is_not_a_number_refuses_the_run() {
    let refusal = Spike::read(&env(&[(ENABLE_ENV, "1"), (DELAY_ENV, "25s")]))
        .expect_err("`25s` is not a count of seconds");
    let ProviderError::Transport(message) = refusal else { panic!("a transport refusal") };
    assert!(message.contains(DELAY_ENV), "{message}");
}

/// The one refusal that is load-bearing rather than tidy: a `SCHEMA=6` typo
/// silently read as `both` would answer measurement (e) with the one answer
/// nobody could tell was wrong.
#[test]
fn a_schema_selector_outside_the_three_refuses_the_run() {
    let refusal = Spike::read(&env(&[(ENABLE_ENV, "1"), (SCHEMA_ENV, "json")]))
        .expect_err("`json` is not one of the three");
    let ProviderError::Transport(message) = refusal else { panic!("a transport refusal") };
    assert!(message.contains(SCHEMA_ENV), "{message}");
    assert!(message.contains("`both`, `3` or `6`"), "and what would have been read: {message}");
}

#[test]
fn a_prompt_mode_outside_append_refuses_the_run() {
    let refusal = Spike::read(&env(&[(ENABLE_ENV, "1"), (PROMPT_ENV, "replace")]))
        .expect_err("only append is built");
    let ProviderError::Transport(message) = refusal else { panic!("a transport refusal") };
    assert!(message.contains(PROMPT_ENV), "{message}");
}

#[test]
fn every_flag_set_reads_back_as_the_run_it_describes() {
    let spike = Spike::read(&env(&[
        (ENABLE_ENV, "true"),
        (DELAY_ENV, "25"),
        (EXEC_HEARTBEAT_ENV, "1"),
        (SCHEMA_ENV, "6"),
        (PROMPT_ENV, "append"),
    ]))
    .expect("a full environment is readable")
    .expect("enabled");

    assert_eq!(spike.delay, Duration::from_secs(25));
    assert!(spike.exec_heartbeat);
    assert_eq!(spike.schema, SchemaFields::Json);
    assert!(spike.prompt_append);
}

#[test]
fn the_declaration_names_one_tool_on_both_schema_fields() {
    let tools = plain().tools();
    assert_eq!(tools.len(), 1, "one tool, so a call can only be about that one");

    let tool = &tools[0];
    assert_eq!(tool.name.as_deref(), Some(TOOL_NAME));
    assert_eq!(tool.tool_name.as_deref(), Some(TOOL_NAME));
    assert_eq!(tool.provider_identifier.as_deref(), Some("ganja"));
    assert_eq!(tool.description.as_deref(), Some("Answers pong. A probe."));
    assert_eq!(
        field_numbers(&tool.encode_to_vec()),
        vec![1, 2, 3, 4, 5, 6],
        "name, description, input_schema, provider_identifier, tool_name, input_schema_json"
    );
}

#[test]
fn the_typed_selector_fills_field_three_alone() {
    let spike =
        Spike::read(&env(&[(ENABLE_ENV, "1"), (SCHEMA_ENV, "3")])).expect("readable").expect("on");
    let tools = spike.tools();

    assert!(tools[0].input_schema.is_set(), "the google.protobuf.Value form");
    assert_eq!(tools[0].input_schema_json, None, "and not the string beside it");
    assert_eq!(field_numbers(&tools[0].encode_to_vec()), vec![1, 2, 3, 4, 5]);
}

#[test]
fn the_json_selector_fills_field_six_alone() {
    let spike =
        Spike::read(&env(&[(ENABLE_ENV, "1"), (SCHEMA_ENV, "6")])).expect("readable").expect("on");
    let tools = spike.tools();

    assert!(!tools[0].input_schema.is_set(), "not the typed form");
    assert_eq!(tools[0].input_schema_json.as_deref(), Some(SCHEMA_JSON));
    assert_eq!(field_numbers(&tools[0].encode_to_vec()), vec![1, 2, 4, 5, 6]);
}

/// The two encodings are derived from one string, so they cannot disagree —
/// which is what keeps a `NO` on measurement (e) a fact about the server
/// rather than about this file.
#[test]
fn the_two_schema_encodings_describe_the_same_document() {
    let tool = &plain().tools()[0];
    let spelled: serde_json::Value =
        serde_json::from_str(tool.input_schema_json.as_deref().expect("the string form"))
            .expect("the string form is JSON");

    assert_eq!(
        tool.input_schema.as_option().expect("the typed form"),
        &json_value(&spelled),
        "the typed form is the string form, encoded"
    );
}

#[test]
fn each_scalar_json_value_encodes_at_its_own_field() {
    let null = json_value(&serde_json::Value::Null);
    assert_eq!(null.null_value, Some(0), "the well-known type's one null member");
    assert_eq!(field_numbers(&null.encode_to_vec()), vec![1]);

    let number = json_value(&serde_json::json!(2.5));
    assert!((number.number_value.expect("a number") - 2.5).abs() < f64::EPSILON);
    assert_eq!(field_numbers(&number.encode_to_vec()), vec![2]);

    let text = json_value(&serde_json::json!("object"));
    assert_eq!(text.string_value.as_deref(), Some("object"));
    assert_eq!(field_numbers(&text.encode_to_vec()), vec![3]);

    let flag = json_value(&serde_json::json!(false));
    assert_eq!(flag.bool_value, Some(false));
    assert_eq!(field_numbers(&flag.encode_to_vec()), vec![4]);
}

#[test]
fn a_json_object_encodes_as_a_struct_of_its_members_in_order() {
    let encoded = json_value(&serde_json::json!({"type": "object", "strict": true}));
    let members = encoded.struct_value.as_option().expect("an object is a struct");

    // serde_json's default map is sorted, and the repeated-entry spelling
    // preserves whatever order it was built in — which is why these bytes are
    // assertable at all where a `map<string, Value>` would not be.
    assert_eq!(
        members.fields.iter().map(|field| field.key.as_deref()).collect::<Vec<_>>(),
        vec![Some("strict"), Some("type")]
    );
    assert_eq!(members.fields[0].value.as_option().expect("a value").bool_value, Some(true));
    assert_eq!(
        members.fields[1].value.as_option().expect("a value").string_value.as_deref(),
        Some("object")
    );
    assert_eq!(field_numbers(&encoded.encode_to_vec()), vec![5]);
}

#[test]
fn a_nested_document_encodes_all_the_way_down() {
    let encoded = json_value(&serde_json::json!({"properties": {"of": ["a", 1]}}));
    let outer = encoded.struct_value.as_option().expect("a struct");
    let inner = outer.fields[0]
        .value
        .as_option()
        .expect("a value")
        .struct_value
        .as_option()
        .expect("a nested struct");
    let list = inner.fields[0]
        .value
        .as_option()
        .expect("a value")
        .list_value
        .as_option()
        .expect("an array is a list");

    assert_eq!(list.values.len(), 2, "an array is never silently dropped");
    assert_eq!(list.values[0].string_value.as_deref(), Some("a"));
    assert!((list.values[1].number_value.expect("a number") - 1.0).abs() < f64::EPSILON);
}

#[test]
fn the_run_heartbeat_is_an_empty_message_at_field_seven() {
    let bytes = run_heartbeat();
    assert_eq!(field_numbers(&bytes), vec![7]);

    let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("what is sent decodes");
    assert!(decoded.client_heartbeat.is_set(), "present, and carrying nothing");
    assert!(decoded.run_request.as_option().is_none(), "a heartbeat is not a second run request");
}

#[test]
fn an_exec_heartbeat_carries_the_exec_it_keeps_alive() {
    let bytes = exec_heartbeat(Some(11));
    let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("what is sent decodes");
    let control = decoded.exec_control.as_option().expect("the control channel");

    assert_eq!(control.heartbeat.as_option().and_then(|beat| beat.id), Some(11));
    assert!(control.stream_close.as_option().is_none(), "a heartbeat does not end the exec");
    assert!(control.throw.as_option().is_none());
    assert_eq!(field_numbers(&control.encode_to_vec()), vec![3]);
}

#[test]
fn the_probe_is_answered_with_one_word_and_then_the_close() {
    let ask = decode::ExecRefusal {
        id: Some(4),
        exec_id: Some("exec-ping".to_owned()),
        kind: "mcp_args".to_owned(),
        arm: decode::RefusalArm::Mcp {
            name: TOOL_NAME.to_owned(),
            tool_call_id: "call-1".to_owned(),
        },
    };

    let sent = pong(&ask)
        .iter()
        .map(|message| {
            proto::ClientMessage::decode_from_slice(message).expect("what is sent decodes")
        })
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 2, "the result, and the close that ends every exec");

    let answer = sent[0].exec_response.as_option().expect("the result rides the exec channel");
    assert_eq!(answer.id, Some(4));
    assert_eq!(answer.exec_id.as_deref(), Some("exec-ping"));
    let success = answer
        .mcp_result
        .as_option()
        .expect("the mcp result")
        .success
        .as_option()
        .expect("the success arm, not the rejection W1 sends");
    assert_eq!(success.is_error, Some(false));
    assert_eq!(
        success.content[0].text.as_option().and_then(|text| text.text.as_deref()),
        Some("pong")
    );

    let closed = sent[1].exec_control.as_option().expect("the close rides the control channel");
    assert_eq!(closed.stream_close.as_option().and_then(|close| close.id), Some(4));
    assert!(closed.throw.as_option().is_none(), "a served exec is not also a broken client");
}

#[test]
fn only_the_declared_probe_is_recognised_as_one() {
    let mcp = |name: &str| decode::ExecRefusal {
        id: Some(1),
        exec_id: None,
        kind: "mcp_args".to_owned(),
        arm: decode::RefusalArm::Mcp { name: name.to_owned(), tool_call_id: String::new() },
    };

    assert!(is_ping(&mcp(TOOL_NAME)));
    assert!(!is_ping(&mcp("ganja_ping_2")), "a near miss is somebody else's tool");
    assert!(!is_ping(&mcp("")), "and so is an unnamed call");
    assert!(
        !is_ping(&decode::ExecRefusal {
            id: Some(1),
            exec_id: None,
            kind: "read_args".to_owned(),
            arm: decode::RefusalArm::Read { path: "Cargo.toml".to_owned() },
        }),
        "a native kind keeps W1's typed refusal, which is what measurement (c) reads"
    );
}

#[test]
fn the_prompt_marker_rides_the_append_arm_alone() {
    assert!(plain().prompt_spec().is_none(), "field 29 is absent unless it is being measured");

    let spike = Spike::read(&env(&[(ENABLE_ENV, "1"), (PROMPT_ENV, "append")]))
        .expect("readable")
        .expect("on");
    let spec = spike.prompt_spec().expect("the spec");

    assert_eq!(spec.append.as_deref(), Some(MARKER));
    assert_eq!(spec.replace, None, "a replace would discard the prompt cloud_rule carries");
    assert_eq!(field_numbers(&spec.encode_to_vec()), vec![2]);
}

/// **AC-9.** The spike's flags reach no config surface, no JSON schema and
/// no `--help` — and no source file outside the cursor wire's own.
///
/// A repo walk is the honest mechanism here, and the only one. The claim is
/// about *absence* across the whole workspace — a config key that had grown
/// somewhere else, a `--help` line, a schema property — and absence cannot be
/// asserted by calling anything: there is no function whose result changes
/// when a second reader appears in `ganja-cli`. Grepping the sources is
/// exactly the check a reviewer would run by hand, so it is written down and
/// run on every build instead.
///
/// **What the allowed set is, and why it is a directory rather than one
/// file.** The plan's wording named `cursor.rs`; the landed shape is the
/// cursor wire's own subtree, because three files there name the flag in
/// prose rather than reading it — `cursor.proto`'s spike section, the
/// generated code that carries those comments, and `cursor.rs`'s own field
/// doc — and a spike documented where it lives is the point rather than a
/// leak. The claim that matters is unchanged and is what fails here: every
/// other crate, every other module, and `schema/` name it nowhere. The single
/// **read** is still `Spike::from_env`, called once from
/// `CursorWire::from_stored`.
///
/// W3a deletes this file, this test, and every path it guards.
#[test]
fn the_spike_flag_is_named_outside_the_cursor_wire_nowhere() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels under the workspace root")
        .to_owned();
    let wire = root.join("crates/ganja-provider/src/provider");

    let mut named = Vec::new();
    let mut walk = |directory: std::path::PathBuf| {
        let mut pending = vec![directory];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                // The cursor wire's own files: `cursor.rs`, `cursor.proto`,
                // and everything under `cursor/`, which is where the spike
                // lives and is documented.
                let mine = path.parent() == Some(wire.as_path())
                    && path.file_stem().is_some_and(|stem| stem == "cursor")
                    || path.parent().is_some_and(|parent| parent == wire.join("cursor"));
                if !mine
                    && std::fs::read_to_string(&path).is_ok_and(|text| text.contains(ENABLE_ENV))
                {
                    named.push(path);
                }
            }
        }
    };

    for crate_dir in std::fs::read_dir(root.join("crates")).expect("the workspace has crates") {
        walk(crate_dir.expect("a readable entry").path().join("src"));
    }
    walk(root.join("schema"));

    assert!(named.is_empty(), "the spike's flag is named outside the cursor wire: {named:?}");
}
