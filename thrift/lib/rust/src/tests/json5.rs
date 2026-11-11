/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use bytes::Bytes;

use crate::ProtocolReader;
use crate::TType;
use crate::json5_protocol;
use crate::json5_protocol::Json5ProtocolDeserializer;
use crate::json5_protocol::Json5WriterOptions;

struct FloatSet(Vec<f64>);

impl crate::GetTType for FloatSet {
    const TTYPE: TType = TType::Set;
}

impl<P: crate::ProtocolWriter> crate::Serialize<P> for FloatSet {
    fn rs_thrift_write(&self, protocol: &mut P) {
        protocol.write_set_begin(TType::Double, self.0.len());
        for value in &self.0 {
            protocol.write_set_value_begin();
            protocol.write_double(*value);
        }
        protocol.write_set_end();
    }
}

#[test]
fn accepts_json5_syntax_and_compatibility_scalars() {
    let input = br#"
        {
          // JSON5 comments and identifiers are accepted.
          numbers: [+1, 0x2a, '3',],
        }
    "#;

    let value: BTreeMap<String, Vec<i64>> =
        json5_protocol::deserialize(&input[..]).expect("valid JSON5");

    assert_eq!(value["numbers"], [1, 42, 3]);

    let hex: Vec<i64> = json5_protocol::deserialize(
        "[+0x1, -0x1, 0xdeadbeef, 0x7fffffffffffffff, -0x8000000000000000]",
    )
    .expect("signed and i64-width JSON5 hex integers");
    assert_eq!(hex, [1, -1, 0xdeadbeef, i64::MAX, i64::MIN]);

    let infinity: f64 = json5_protocol::deserialize("+Infinity").expect("positive infinity");
    let nan: f64 = json5_protocol::deserialize("+NaN").expect("positive NaN");
    assert_eq!(infinity, f64::INFINITY);
    assert!(nan.is_nan() && nan.is_sign_positive());
}

#[test]
fn skips_empty_collections_without_end_callbacks() {
    for (input, collection_type) in [("{}", TType::Map), ("[]", TType::Set), ("[]", TType::List)] {
        let mut reader = Json5ProtocolDeserializer::new(input.as_bytes());
        reader
            .skip(collection_type)
            .expect("empty collection must be skipped");
    }
}

#[test]
fn serializes_complex_map_keys_as_key_value_objects() {
    let value = BTreeMap::from([
        (10_i32, "ten".to_owned()),
        (2, "two".to_owned()),
        (-2, "negative two".to_owned()),
        (-10, "negative ten".to_owned()),
    ]);

    let encoded = json5_protocol::serialize(value);

    assert_eq!(
        encoded,
        r#"[{"key":-10,"value":"negative ten"},{"key":-2,"value":"negative two"},{"key":2,"value":"two"},{"key":10,"value":"ten"}]"#
    );
    assert_eq!(
        json5_protocol::serialize(BTreeMap::<i32, String>::new()),
        "[]",
        "an empty non-string-key map still uses key-value array form"
    );
}

#[test]
fn serializes_binary_using_utf8_or_base64url_wrapper() {
    assert_eq!(
        json5_protocol::serialize(Bytes::from_static(b"hello")),
        r#"{"utf-8":"hello"}"#
    );
    assert_eq!(
        json5_protocol::serialize(Bytes::from_static(&[0xff, 0x00])),
        r#"{"base64url":"_wA"}"#
    );
    assert_eq!(
        json5_protocol::serialize(Bytes::from_static(b"hello\0")),
        r#"{"base64url":"aGVsbG8A"}"#,
        "valid but non-printable UTF-8 must use base64url"
    );

    let utf8: Bytes =
        json5_protocol::deserialize(r#"{"utf-8":"hello"}"#).expect("valid UTF-8 binary wrapper");
    let base64url: Bytes = json5_protocol::deserialize(r#"{"base64url":"_wA="}"#)
        .expect("valid padded base64url binary wrapper");
    assert_eq!(utf8, Bytes::from_static(b"hello"));
    assert_eq!(base64url, Bytes::from_static(&[0xff, 0x00]));
    assert!(
        json5_protocol::deserialize::<Bytes, _, _>(r#""QQ=""#).is_err(),
        "base64 padding must be complete when present"
    );
}

#[test]
fn serializes_binary_as_base64_strings_when_enabled() {
    for options in [
        Json5WriterOptions::default(),
        Json5WriterOptions::basic_json(),
        Json5WriterOptions::json5(),
    ] {
        for (input, expected) in [
            (&b""[..], r#""""#),
            (&b"hello"[..], r#""aGVsbG8""#),
            (&b"hello\0"[..], r#""aGVsbG8A""#),
            (&[0xff][..], r#""/w""#),
            (&[0xfb, 0xff][..], r#""+/8""#),
        ] {
            let value = Bytes::copy_from_slice(input);
            let encoded = json5_protocol::serialize_with_options(
                value.clone(),
                options.with_binary_as_base64_string(true),
            );
            assert_eq!(encoded, expected);
            let decoded: Bytes =
                json5_protocol::deserialize(&encoded).expect("base64 output must round-trip");
            assert_eq!(decoded, value);
            assert_eq!(
                json5_protocol::serialize_with_options(
                    value.clone(),
                    options
                        .with_binary_as_base64_string(true)
                        .with_binary_as_base64_string(false),
                ),
                json5_protocol::serialize_with_options(value, options),
                "disabling the option restores tagged binary output"
            );
        }
    }
}

#[test]
fn preserves_float_precision_and_negative_zero() {
    let values = vec![0.100_000_01_f32, -0.0];

    assert_eq!(json5_protocol::serialize(values), "[0.10000001,-0.0]");
    assert_eq!(
        json5_protocol::serialize(-f64::NAN),
        r#""-NaN""#,
        "the sign of NaN is part of the canonical representation"
    );
}

#[test]
fn emits_json5_mode_and_round_trips_special_floats() {
    let options = Json5WriterOptions::json5()
        .with_indent_width(2)
        .expect("supported indentation");
    let encoded = json5_protocol::serialize_with_options(
        BTreeMap::from([
            ("finite".to_owned(), 1.5),
            ("infinite".to_owned(), f64::INFINITY),
        ]),
        options,
    );

    assert_eq!(encoded, "{\n  finite: 1.5,\n  infinite: Infinity,\n}");
    let decoded: BTreeMap<String, f64> =
        json5_protocol::deserialize(&encoded).expect("JSON5 output must be readable");
    assert_eq!(decoded["finite"], 1.5);
    assert_eq!(decoded["infinite"], f64::INFINITY);
    let negative_nan: f64 = json5_protocol::deserialize("-NaN").expect("valid signed JSON5 NaN");
    assert!(negative_nan.is_nan());
    assert!(
        negative_nan.is_sign_negative(),
        "deserialization must preserve the NaN sign"
    );

    for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, -f64::NAN] {
        let encoded = json5_protocol::serialize(value);
        let decoded: f64 = json5_protocol::deserialize(&encoded)
            .expect("basic JSON special float output must be readable");
        assert_eq!(decoded.is_nan(), value.is_nan());
        assert_eq!(decoded.is_sign_negative(), value.is_sign_negative());
        if !value.is_nan() {
            assert_eq!(decoded, value);
        }
    }
}

#[test]
fn json5_identifier_keys_do_not_interfere_with_number_parsing() {
    let value = BTreeMap::from([("NaN_value".to_owned(), 1_i32)]);
    let encoded =
        json5_protocol::serialize_with_options(value.clone(), Json5WriterOptions::json5());
    assert_eq!(encoded, "{NaN_value:1,}");
    assert_eq!(
        json5_protocol::deserialize::<BTreeMap<String, i32>, _, _>(&encoded)
            .expect("writer output must round-trip"),
        value
    );

    let commented: BTreeMap<String, i32> =
        json5_protocol::deserialize("{Infinity /* comment */: 1, NaN // comment\n: 2}")
            .expect("comments are legal before an object-name colon");
    assert_eq!(
        commented,
        BTreeMap::from([("Infinity".to_owned(), 1), ("NaN".to_owned(), 2)])
    );
}

#[test]
fn hash_map_output_is_deterministic() {
    let value = HashMap::from([
        ("third".to_owned(), 3_i32),
        ("first".to_owned(), 1),
        ("second".to_owned(), 2),
    ]);

    let encoded = json5_protocol::serialize(value);

    assert_eq!(encoded, r#"{"first":1,"second":2,"third":3}"#);
}

#[test]
fn float_set_output_is_deterministic_with_nan() {
    let first = json5_protocol::serialize(FloatSet(vec![f64::NAN, 2.0, 1.0]));
    let second = json5_protocol::serialize(FloatSet(vec![2.0, f64::NAN, 1.0]));

    assert_eq!(first, second);
    assert_eq!(first, r#"[1.0,2.0,"NaN"]"#);
}

#[test]
fn deserializes_empty_collections() {
    assert_eq!(
        json5_protocol::deserialize::<Vec<i32>, _, _>("[]").expect("empty list"),
        Vec::<i32>::new()
    );
    assert_eq!(
        json5_protocol::deserialize::<BTreeSet<i32>, _, _>("[]").expect("empty set"),
        BTreeSet::new()
    );
    assert_eq!(
        json5_protocol::deserialize::<BTreeMap<String, i32>, _, _>("{}")
            .expect("empty string-key map"),
        BTreeMap::new()
    );
    assert_eq!(
        json5_protocol::deserialize::<BTreeMap<i32, i32>, _, _>("[]")
            .expect("empty non-string-key map"),
        BTreeMap::new()
    );
}

#[test]
fn rejects_malformed_or_excessively_nested_input() {
    let malformed = json5_protocol::deserialize::<Vec<i32>, _, _>("[1,,2]");
    assert!(malformed.is_err(), "malformed JSON5 must be rejected");

    let nested = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    let too_deep = json5_protocol::deserialize::<i32, _, _>(nested);
    assert_eq!(
        too_deep
            .expect_err("excessive nesting must be rejected")
            .to_string(),
        "protocol skip depth exceeded"
    );

    let unicode_line_ending = format!("// comment\u{2028}{}0{}", "[".repeat(65), "]".repeat(65));
    let bypass = json5_protocol::deserialize::<i32, _, _>(unicode_line_ending);
    assert_eq!(
        bypass
            .expect_err("Unicode line endings must not bypass the depth guard")
            .to_string(),
        "protocol skip depth exceeded"
    );

    let maximum = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    let mut reader = Json5ProtocolDeserializer::new(maximum.as_bytes());
    reader
        .skip(TType::List)
        .expect("the documented maximum nesting depth must be accepted");
}

#[test]
fn rejects_lossy_integer_to_float_conversions() {
    assert!(
        json5_protocol::deserialize::<f32, _, _>("123456789").is_err(),
        "integer precision loss must be rejected for float"
    );
    assert!(
        json5_protocol::deserialize::<f64, _, _>("9007199254740993").is_err(),
        "integer precision loss must be rejected for double"
    );
    assert!(
        json5_protocol::deserialize::<f32, _, _>("1e100").is_err(),
        "finite values must not overflow to infinity"
    );
    assert_eq!(
        json5_protocol::deserialize::<f32, _, _>("1.0000000596046448").expect("valid float"),
        f32::from_bits(1.0_f32.to_bits() + 1),
        "float parsing must round once at single precision"
    );
}

#[test]
fn rejects_unsafe_indentation() {
    assert!(
        Json5WriterOptions::json5()
            .with_indent_width(usize::MAX)
            .is_err(),
        "indentation must be bounded before output allocation"
    );
}
