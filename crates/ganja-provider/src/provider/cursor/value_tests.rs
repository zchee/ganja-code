use serde_json::json;

use super::{arguments, decode, encode};
use crate::provider::cursor::proto;

/// Every `serde_json` variant survives the trip out and back, nesting
/// included. Outbound this is a tool's schema and inbound it is the arguments
/// a model called with, so a variant that did not round-trip would either
/// declare a schema nobody wrote or run a tool on values nobody chose.
#[test]
fn every_json_shape_survives_the_trip_out_and_back() {
    for value in [
        json!(null),
        json!(true),
        json!(false),
        json!(0),
        json!(-17),
        json!(1.5),
        json!(""),
        json!("a string"),
        json!([]),
        json!({}),
        json!([1, "two", null, {"three": [4]}]),
        json!({
            "type": "object",
            "properties": { "filePath": { "type": "string" }, "limit": { "type": "number" } },
            "required": ["filePath"],
            "additionalProperties": false,
        }),
    ] {
        assert_eq!(decode(&encode(&value)), Some(value.clone()), "{value}");
    }
}

/// A null is the field carrying `google.protobuf.NullValue`'s one member,
/// which is `0` — the encoding a reader could mistake for "nothing was set" if
/// this build wrote it any other way.
#[test]
fn a_null_is_the_zero_member_of_the_well_known_enum() {
    let encoded = encode(&json!(null));
    assert_eq!(encoded.null_value, Some(0));
    assert!(encoded.string_value.is_none() && encoded.bool_value.is_none());
}

/// The decoder refuses rather than guesses. On the wire this message is a
/// oneof, and a oneof always writes its selected case — so nothing set at all
/// is a shape this build cannot read, and reading it as a null would hand a
/// tool an argument the model never wrote.
#[test]
fn a_value_with_no_arm_set_is_unreadable_rather_than_null() {
    assert_eq!(decode(&proto::JsonValue::default()), None);
}

/// Two arms at once is a message no oneof encoder produces, so it is refused
/// rather than resolved by a precedence rule nobody has seen the server use.
#[test]
fn a_value_with_two_arms_set_is_unreadable() {
    let confused = proto::JsonValue {
        string_value: Some("text".to_owned()),
        bool_value: Some(true),
        ..Default::default()
    };

    assert_eq!(decode(&confused), None);
}

/// A number the wire can carry and JSON cannot has no honest spelling, so the
/// call it arrived on fails rather than the value being invented.
#[test]
fn a_number_json_cannot_spell_is_unreadable() {
    let nan = proto::JsonValue { number_value: Some(f64::NAN), ..Default::default() };
    let infinite = proto::JsonValue { number_value: Some(f64::INFINITY), ..Default::default() };

    assert_eq!(decode(&nan), None);
    assert_eq!(decode(&infinite), None);
}

/// An unreadable member anywhere in a nested shape makes the whole value
/// unreadable: a struct that quietly dropped the member it could not read
/// would call a tool with arguments the model did not write.
#[test]
fn an_unreadable_member_makes_the_whole_shape_unreadable() {
    let nested = proto::JsonValue {
        struct_value: buffa::MessageField::some(proto::JsonStruct {
            fields: vec![proto::JsonField {
                key: Some("broken".to_owned()),
                value: buffa::MessageField::some(proto::JsonValue::default()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let listed = proto::JsonValue {
        list_value: buffa::MessageField::some(proto::JsonList {
            values: vec![proto::JsonValue::default()],
            ..Default::default()
        }),
        ..Default::default()
    };

    assert_eq!(decode(&nested), None);
    assert_eq!(decode(&listed), None);
}

/// A member with no key at all cannot become an object member: naming it for
/// the server would be this build writing an argument name.
#[test]
fn a_struct_member_with_no_key_is_unreadable() {
    let anonymous = proto::JsonValue {
        struct_value: buffa::MessageField::some(proto::JsonStruct {
            fields: vec![proto::JsonField {
                value: buffa::MessageField::some(encode(&json!(1))),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };

    assert_eq!(decode(&anonymous), None);
}

/// The argument map an `mcp_args` carries, read as the object a tool call
/// runs with — and an empty map as the empty object, because a tool whose
/// schema declares no arguments is called with none. That is the shape the
/// live probe measured: `args = 2` absent on every call to an argument-less
/// declaration.
#[test]
fn an_argument_map_reads_as_the_object_a_tool_call_runs_with() {
    assert_eq!(arguments(&[]), Some(json!({})));

    let entries = vec![
        proto::McpArgEntry {
            key: Some("filePath".to_owned()),
            value: buffa::MessageField::some(encode(&json!("/repo/src/lib.rs"))),
            ..Default::default()
        },
        proto::McpArgEntry {
            key: Some("limit".to_owned()),
            value: buffa::MessageField::some(encode(&json!(40))),
            ..Default::default()
        },
    ];

    assert_eq!(
        arguments(&entries),
        Some(json!({ "filePath": "/repo/src/lib.rs", "limit": 40 })),
        "the map is the tool's own argument object"
    );
}

/// One unreadable entry fails the whole map, which is what fails that one
/// call — never the turn.
#[test]
fn an_argument_map_with_an_unreadable_value_is_unreadable() {
    let entries = vec![proto::McpArgEntry {
        key: Some("filePath".to_owned()),
        value: buffa::MessageField::some(proto::JsonValue::default()),
        ..Default::default()
    }];

    assert_eq!(arguments(&entries), None);
}

/// The one place the round trip is not the identity, and the reason it must
/// not be: `google.protobuf.Value` carries every number as a **double**, so a
/// model that wrote `40` arrives as `40.0` — and `serde_json` will not read a
/// `u64` out of a float, which would refuse `read`'s own `limit` for a value
/// the model spelled correctly. JSON draws no distinction between the two, so
/// the integer spelling is restored.
#[test]
fn an_integral_number_comes_back_as_an_integer_a_tool_can_deserialize() {
    let forty = decode(&encode(&json!(40))).expect("a number reads");
    assert!(forty.is_u64(), "a tool's `limit: u64` has to deserialize from this: {forty}");

    #[derive(serde::Deserialize)]
    struct Window {
        limit: u64,
    }
    let window: Window = serde_json::from_value(
        arguments(&[proto::McpArgEntry {
            key: Some("limit".to_owned()),
            value: buffa::MessageField::some(encode(&json!(2000))),
            ..Default::default()
        }])
        .expect("the map reads"),
    )
    .expect("a tool's own argument struct deserializes from what the wire carried");
    assert_eq!(window.limit, 2000);

    assert!(decode(&encode(&json!(1.5))).expect("a number reads").is_f64(), "a real stays real");
}
