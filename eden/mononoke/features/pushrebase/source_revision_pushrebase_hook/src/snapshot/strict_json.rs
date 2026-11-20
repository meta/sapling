/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! JSON parsing that rejects an object with the same key twice, which
//! `serde_json` would otherwise resolve by keeping the last value, hiding the
//! others from the check.

use std::fmt;

use serde::Deserialize;
use serde::Deserializer;
use serde::de;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Map;
use serde_json::Number;
use serde_json::Value;

/// `serde_json::from_slice`, except that an object with a repeated key is an
/// error.
pub(crate) fn from_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    serde_json::from_slice::<Strict>(bytes).map(|strict| strict.0)
}

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(Strict(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let Strict(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn test_parses_like_serde_json() {
        for json in [
            r#"{"a": 1, "b": [1, -2, 3.5, "x", true, null, {"c": {}}]}"#,
            "18446744073709551615",
            "18446744073709551616",
            "-9223372036854775808",
            "1.0",
            "1e3",
            "[]",
        ] {
            assert_eq!(
                from_slice(json.as_bytes()).ok(),
                serde_json::from_slice::<Value>(json.as_bytes()).ok(),
                "{json}"
            );
        }
    }

    #[mononoke::test]
    fn test_rejects_repeated_keys_at_any_depth() {
        for json in [
            r#"{"a": 1, "a": 1}"#,
            r#"{"a": 1, "a": 2}"#,
            r#"{"modules": [{"strings": [{"key": "K", "key": "L"}]}]}"#,
            r#"[{"x": {"y": 1, "y": 2}}]"#,
        ] {
            let error = from_slice(json.as_bytes()).expect_err(json);
            assert!(
                error.to_string().contains("duplicate key"),
                "{json}: {error}"
            );
        }
    }

    #[mononoke::test]
    fn test_rejects_non_finite_and_invalid_numbers() {
        for json in [
            "NaN",
            "Infinity",
            "-Infinity",
            r#"{"a": NaN}"#,
            "1e400",
            "01",
            "1.",
        ] {
            assert!(from_slice(json.as_bytes()).is_err(), "{json}");
        }
    }
}
