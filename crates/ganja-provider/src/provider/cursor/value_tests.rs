use buffa::Message as _;
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

/// One nested shape's **bytes**, against the field numbers cursor's own
/// descriptor carries — not against this module's decoder.
///
/// Every other test here is a round trip, and a round trip between two halves
/// of one file cannot see a transposition: swap `JsonStruct.fields` for
/// `JsonList.values`, or `JsonValue`'s `struct_value` for its `list_value`, and
/// `encode`/`decode` still agree with each other while the wire is wrong.
/// Nothing else in the tree closes that gap — `value::encode` has no shipped
/// caller (the roster declares on `input_schema_json = 6`), so
/// `ganja-testkit`'s mock server builds its `McpArgs.args` with this very
/// encoder, and the live probe recorded `args = 2` **absent** on every run, so
/// there is no recording of a populated argument map either. This assertion is
/// the pin.
///
/// The expected bytes are written out by hand from the descriptor
/// (`index.js@3928104` for `google.protobuf.Value`, `@3926700` for `Struct`,
/// `@3929084` for `ListValue`, as `cursor.proto` cites them):
/// `JsonValue` numbers its arms 1-6, `JsonStruct.fields = 1`,
/// `JsonField{key = 1, value = 2}`, `JsonList.values = 1`. A tag byte is
/// `(number << 3) | wire_type`, where `0` is varint, `1` is fixed64 and `2` is
/// length-delimited.
///
/// Member order is `serde_json::Map`'s, which is a `BTreeMap` here — this
/// workspace does not enable `preserve_order` — so it is alphabetical, and the
/// encoder has no ordering choice of its own to get wrong.
#[test]
fn a_nested_value_encodes_the_bytes_the_descriptor_numbers_spell() {
    let value = json!({
        "flag": true,
        "items": [2.5],
        "name": "hi",
        "nothing": null,
        "size": 7,
    });

    let expected: Vec<u8> = [
        // JsonValue.struct_value = 5, length-delimited: tag 0x2a, then 84
        // bytes of JsonStruct. Each member below is one JsonStruct.fields = 1
        // entry (tag 0x0a, length), holding JsonField.key = 1 (tag 0x0a) and
        // JsonField.value = 2 (tag 0x12).
        &b"\x2a\x54"[..],
        // "flag": true — JsonValue.bool_value = 4, varint: tag 0x20, then 1.
        &b"\x0a\x0a\x0a\x04flag\x12\x02\x20\x01"[..],
        // "items": [2.5] — JsonValue.list_value = 6 (tag 0x32) wrapping
        // JsonList.values = 1 (tag 0x0a) wrapping JsonValue.number_value = 2,
        // fixed64 (tag 0x11): 2.5 is 0x4004000000000000, little-endian.
        &b"\x0a\x16\x0a\x05items\x12\x0d\x32\x0b\x0a\x09\x11\x00\x00\x00\x00\x00\x00\x04\x40"[..],
        // "name": "hi" — JsonValue.string_value = 3, length-delimited: tag
        // 0x1a, length 2.
        &b"\x0a\x0c\x0a\x04name\x12\x04\x1a\x02hi"[..],
        // "nothing": null — JsonValue.null_value = 1, varint: tag 0x08 and the
        // enum's one member, 0. Written *because* the field is optional in the
        // generated type: a zero elided here would be indistinguishable from
        // an empty message, which `decode` refuses as unreadable.
        &b"\x0a\x0d\x0a\x07nothing\x12\x02\x08\x00"[..],
        // "size": 7 — the same fixed64 arm as `items`' element, because the
        // well-known type has only a double: 7.0 is 0x401c000000000000.
        &b"\x0a\x11\x0a\x04size\x12\x09\x11\x00\x00\x00\x00\x00\x00\x1c\x40"[..],
    ]
    .concat();

    assert_eq!(
        encode(&value).encode_to_vec(),
        expected,
        "the encoder writes the field numbers the descriptor declares",
    );

    // And the other direction off the *same* hand-written bytes, so the
    // decoder is pinned to the wire too rather than only to its own encoder.
    let read = proto::JsonValue::decode_from_slice(&expected).expect("the pinned bytes decode");
    assert_eq!(decode(&read), Some(value), "and the decoder reads them back as what was written");
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
