//! `google.protobuf.Value` in both directions.
//!
//! `pub`, alone among this wire's modules, because the argument map an
//! `mcp_args` carries is the one shape a test outside this crate has to be
//! able to **build**: an engine-level test of the bridge is a mock cursor
//! server calling a ganja tool with real arguments, and it encodes them with
//! the same function this wire decodes them with.
//!
//! Spec: the well-known type's own definition, as cursor's descriptor carries
//! it (`index.js@3928104`, the `2026.09.02-c22c1a3` bundle read recorded in
//! `.omc/research/cursor/2026-09-04-cursor-agent-bundle-read.md`);
//! `cursor.proto`'s [`JsonValue`](proto::JsonValue) says why the oneof is
//! flattened.
//!
//! Two directions, and they are not symmetric. **Outbound** ([`encode`]) turns
//! a tool's JSON schema into the typed form — total, because a variant it
//! dropped would declare a schema the tool does not have. **Inbound**
//! (`decode`, `arguments`, this wire's own) turns the argument values a model called with
//! back into JSON — *partial*, because a shape this build cannot read is not
//! something to guess at: handing a tool arguments the model did not write is
//! worse than answering that one call with an error. A `None` here becomes
//! `McpResult.error` for the call it arrived on, and never a failed turn.

use super::proto;

/// `serde_json`'s value as `google.protobuf.Value`'s flattened shape.
///
/// Total over the input: every `serde_json::Value` variant has an arm, so a
/// schema declared through this reaches the server as the schema that was
/// written.
#[must_use]
pub fn encode(value: &serde_json::Value) -> proto::JsonValue {
    let mut encoded = proto::JsonValue::default();
    match value {
        // `google.protobuf.NullValue` has exactly one member, `0`; sending the
        // field with that value is how the well-known type spells a null.
        serde_json::Value::Null => encoded.null_value = Some(0),
        serde_json::Value::Bool(value) => encoded.bool_value = Some(*value),
        // `as_f64` returns `None` only under serde_json's `arbitrary_precision`
        // feature, which this workspace does not enable; a NaN would still be
        // a number on the wire rather than a dropped field.
        serde_json::Value::Number(value) => {
            encoded.number_value = Some(value.as_f64().unwrap_or(f64::NAN));
        }
        serde_json::Value::String(value) => encoded.string_value = Some(value.clone()),
        serde_json::Value::Array(items) => {
            encoded.list_value = buffa::MessageField::some(proto::JsonList {
                values: items.iter().map(encode).collect(),
                ..Default::default()
            });
        }
        serde_json::Value::Object(members) => {
            encoded.struct_value = buffa::MessageField::some(proto::JsonStruct {
                fields: members
                    .iter()
                    .map(|(key, value)| proto::JsonField {
                        key: Some(key.clone()),
                        value: buffa::MessageField::some(encode(value)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
        }
    }

    encoded
}

/// One `google.protobuf.Value` as JSON, or [`None`] for a shape this build
/// cannot read.
///
/// **Exactly one arm must be set.** On the wire this message is a oneof, and a
/// oneof always serializes its selected case — even when that case's value is
/// the type's zero, which is what makes an explicit `null_value = 0` readable
/// and an *empty* message honestly unreadable rather than a null. Two arms set
/// at once is a message no oneof encoder produces, so it is refused too rather
/// than resolved by a precedence rule nobody has observed the server using.
#[must_use]
pub(super) fn decode(value: &proto::JsonValue) -> Option<serde_json::Value> {
    let arms = usize::from(value.null_value.is_some())
        + usize::from(value.number_value.is_some())
        + usize::from(value.string_value.is_some())
        + usize::from(value.bool_value.is_some())
        + usize::from(value.struct_value.is_set())
        + usize::from(value.list_value.is_set());
    if arms != 1 {
        return None;
    }

    if value.null_value.is_some() {
        return Some(serde_json::Value::Null);
    }
    if let Some(number) = value.number_value {
        return number_value(number).map(serde_json::Value::Number);
    }
    if let Some(text) = &value.string_value {
        return Some(serde_json::Value::String(text.clone()));
    }
    if let Some(flag) = value.bool_value {
        return Some(serde_json::Value::Bool(flag));
    }
    if let Some(fields) = value.struct_value.as_option() {
        return members(fields.fields.iter().map(|field| (&field.key, field.value.as_option())));
    }

    let items = value.list_value.as_option()?;
    let mut decoded = Vec::with_capacity(items.values.len());
    for item in &items.values {
        decoded.push(decode(item)?);
    }

    Some(serde_json::Value::Array(decoded))
}

/// One `number_value` as the JSON number a tool's own deserializer expects.
///
/// **An integral double comes back an integer**, and that is not cosmetic. The
/// well-known type has one number arm and it is a `double`, so a model that
/// wrote `40` reaches this decoder as `40.0` — and `serde_json` will not read a
/// `u64` out of a float, so `read`'s `limit`, `glob`'s counts and every other
/// integer argument would be refused as "invalid type: floating point" for
/// values the model spelled correctly. JSON itself draws no such distinction:
/// `40` and `40.0` are one number, so restoring the integer spelling loses
/// nothing in the spelling — the wire's `double` has already lost anything
/// above 2^53, which no decoder can give back — and is the only spelling both
/// ends agree on. An integral double past `i64`'s range stays a float, since
/// there is no integer it would be.
///
/// [`None`] for a value JSON cannot spell at all — NaN and the infinities —
/// which fails the one call it arrived on rather than inventing a value for it.
fn number_value(number: f64) -> Option<serde_json::Number> {
    // Strict on the right: `i64::MAX as f64` rounds *up* to 2^63, which `as
    // i64` would saturate to `i64::MAX` — one value the model never wrote.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the guard is exactly that the value is an integer inside i64's range"
    )]
    if number.fract() == 0.0 && (i64::MIN as f64) <= number && number < (i64::MAX as f64) {
        return Some(serde_json::Number::from(number as i64));
    }

    serde_json::Number::from_f64(number)
}

/// Keyed members as a JSON object — a `google.protobuf.Struct`'s fields, or
/// an `McpArgs.args` map, which are the same `{key = 1, value = 2}` entry
/// under two message names.
///
/// A member with no key is refused rather than read as `""`: an object whose
/// members this build had to name for it is not the object the model wrote.
fn members<'a>(
    pairs: impl ExactSizeIterator<Item = (&'a Option<String>, Option<&'a proto::JsonValue>)>,
) -> Option<serde_json::Value> {
    let mut object = serde_json::Map::with_capacity(pairs.len());
    for (key, value) in pairs {
        object.insert(key.clone()?, decode(value?)?);
    }

    Some(serde_json::Value::Object(object))
}

/// An `McpArgs.args` map as the JSON object a tool call carries.
///
/// An empty map is an empty object, not a missing one: a tool whose schema
/// declares no arguments is called with none, and `{}` is what its own
/// deserializer expects. That is the shape the live probe's argument-less
/// declaration was called with (the recording's (a), `args = 2` ABSENT).
#[must_use]
pub(super) fn arguments(entries: &[proto::McpArgEntry]) -> Option<serde_json::Value> {
    members(entries.iter().map(|entry| (&entry.key, entry.value.as_option())))
}

#[cfg(test)]
#[path = "value_tests.rs"]
mod tests;
