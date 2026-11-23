//! `Value`: one cell of data, in a shape that is safe to send to the UI.
//!
//! Every driver (Postgres, MySQL, ...) converts what the server returns into a `Value`.
//! The UI only ever sees `Value`, never driver-specific types.
//!
//! Why so many string variants? JavaScript numbers are 64-bit floats, so they silently
//! lose precision above 2^53. To keep data exact we send anything that could lose
//! precision (big integers, decimals, timestamps, JSON) as text, exactly as the server
//! printed it. The UI parses it only when it needs to display or compare.

use serde::{Serialize, Serializer};

/// The JSON shape is `{"type": "int", "value": "123"}` (`value` is omitted for `null`),
/// so TypeScript can switch on `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    Null,
    Bool(bool),
    /// Sent as a string: a JS number cannot hold every i64.
    Int(#[serde(serialize_with = "as_string")] i64),
    /// Sent as a string: a JS number cannot hold every u64.
    UInt(#[serde(serialize_with = "as_string")] u64),
    /// Finite values are plain JSON numbers. NaN and infinities are not valid JSON,
    /// so they are sent as the strings "NaN", "Infinity" and "-Infinity".
    Float(#[serde(serialize_with = "float_safe")] f64),
    /// Exact digits as text, e.g. "0.10". Never convert this to a float.
    Decimal(String),
    Text(String),
    /// Sent as a lowercase hex string (compact, and no extra dependency).
    Bytes(#[serde(serialize_with = "as_hex")] Vec<u8>),
    /// The original JSON text, so key order and number precision are preserved.
    Json(String),
    /// The timestamp text as the server printed it, plus its time zone if known.
    Timestamp {
        iso: String,
        tz: Option<String>,
    },
    Date(String),
    Time(String),
    Uuid(String),
    Array(Vec<Value>),
    /// A type we do not recognise yet: the server's own type name and text form.
    /// Showing something is better than failing.
    Unknown {
        type_name: String,
        text: String,
    },
}

// serde calls these helpers to customise how one field is written.

fn as_string<T: ToString, S: Serializer>(value: &T, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

fn float_safe<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() {
        serializer.serialize_f64(*value)
    } else if value.is_nan() {
        serializer.serialize_str("NaN")
    } else if *value > 0.0 {
        serializer.serialize_str("Infinity")
    } else {
        serializer.serialize_str("-Infinity")
    }
}

fn as_hex<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    serializer.serialize_str(&hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, to_value};

    // A small helper so each test reads as "this Value becomes this JSON".
    fn js(value: Value) -> serde_json::Value {
        to_value(value).expect("Value should always serialize")
    }

    #[test]
    fn null_has_no_value_field() {
        assert_eq!(js(Value::Null), json!({"type": "null"}));
    }

    #[test]
    fn big_integers_are_strings_so_js_cannot_round_them() {
        // 2^53 + 1 is the first integer a JS number cannot represent.
        assert_eq!(
            js(Value::Int(9_007_199_254_740_993)),
            json!({"type": "int", "value": "9007199254740993"})
        );
        assert_eq!(
            js(Value::Int(i64::MIN)),
            json!({"type": "int", "value": "-9223372036854775808"})
        );
        assert_eq!(
            js(Value::UInt(u64::MAX)),
            json!({"type": "u_int", "value": "18446744073709551615"})
        );
    }

    #[test]
    fn decimals_keep_their_exact_digits() {
        // 0.1 and trailing zeros survive because we never go through f64.
        assert_eq!(
            js(Value::Decimal("0.10".into())),
            json!({"type": "decimal", "value": "0.10"})
        );
        assert_eq!(
            js(Value::Decimal("12345678901234567890.123456789".into())),
            json!({"type": "decimal", "value": "12345678901234567890.123456789"})
        );
    }

    #[test]
    fn floats_are_numbers_unless_json_cannot_hold_them() {
        assert_eq!(
            js(Value::Float(1.5)),
            json!({"type": "float", "value": 1.5})
        );
        assert_eq!(
            js(Value::Float(f64::NAN)),
            json!({"type": "float", "value": "NaN"})
        );
        assert_eq!(
            js(Value::Float(f64::INFINITY)),
            json!({"type": "float", "value": "Infinity"})
        );
        assert_eq!(
            js(Value::Float(f64::NEG_INFINITY)),
            json!({"type": "float", "value": "-Infinity"})
        );
    }

    #[test]
    fn bytes_become_lowercase_hex() {
        assert_eq!(
            js(Value::Bytes(vec![0xde, 0xad, 0x00, 0x0f])),
            json!({"type": "bytes", "value": "dead000f"})
        );
        assert_eq!(
            js(Value::Bytes(vec![])),
            json!({"type": "bytes", "value": ""})
        );
    }

    #[test]
    fn json_text_is_kept_verbatim() {
        // Key order and number formatting are the server's, not ours.
        let original = r#"{"b": 1.0, "a": 12345678901234567890}"#;
        assert_eq!(
            js(Value::Json(original.into())),
            json!({"type": "json", "value": original})
        );
    }

    #[test]
    fn timestamps_keep_original_text_and_zone() {
        assert_eq!(
            js(Value::Timestamp {
                iso: "2026-03-29 02:30:00+01".into(),
                tz: Some("Europe/Berlin".into()),
            }),
            json!({"type": "timestamp", "value": {"iso": "2026-03-29 02:30:00+01", "tz": "Europe/Berlin"}})
        );
        assert_eq!(
            js(Value::Timestamp {
                iso: "2026-11-23 10:00:00".into(),
                tz: None,
            }),
            json!({"type": "timestamp", "value": {"iso": "2026-11-23 10:00:00", "tz": null}})
        );
    }

    #[test]
    fn text_survives_emoji_and_quotes() {
        let text = "naïve \"café\" 😀 \n tab\t";
        assert_eq!(
            js(Value::Text(text.into())),
            json!({"type": "text", "value": text})
        );
    }

    #[test]
    fn arrays_nest_and_keep_their_element_types() {
        let value = Value::Array(vec![Value::Int(1), Value::Null, Value::Text("x".into())]);
        assert_eq!(
            js(value),
            json!({"type": "array", "value": [
                {"type": "int", "value": "1"},
                {"type": "null"},
                {"type": "text", "value": "x"}
            ]})
        );
    }

    #[test]
    fn unknown_types_fall_back_to_the_servers_text() {
        assert_eq!(
            js(Value::Unknown {
                type_name: "tsvector".into(),
                text: "'a':1 'b':2".into(),
            }),
            json!({"type": "unknown", "value": {"type_name": "tsvector", "text": "'a':1 'b':2"}})
        );
    }
}
